// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Implicit null checks: the receiver dereference *is* the check.
//!
//! A `getfield` on a null receiver must raise `NullPointerException`. The
//! ordinary way to guarantee that is to emit `TEST RAX, RAX; JZ slow` before
//! the load. The implicit way is to emit nothing, let the load fault on the
//! null page, and translate the `SIGSEGV` / `EXCEPTION_ACCESS_VIOLATION` back
//! into the slow path that raises the NPE.
//!
//! This module owns the faulting-PC → recovery-PC map that makes that
//! translation possible, and the hazards it has to close.
//!
//! # Hazard 1 — the lookup runs in a signal handler
//!
//! It must be async-signal-safe, which rules out the `Mutex` that
//! [`crate::lookup_jit_method_name`] uses. That function is only ever called
//! while the process is already dying, so a `try_lock` there is acceptable;
//! here the process is expected to *survive*, and a handler that blocks on a
//! lock its own interrupted thread holds would deadlock.
//!
//! So the table is a fixed-capacity array of atomics in BSS, sized at compile
//! time, and the reader does nothing but relaxed/acquire loads plus one
//! lock-free `fetch_add` on a counter. There is no allocation, no lock, and no
//! call into anything that takes one. [`recover`] also indexes every array
//! through `slice::get` rather than `[]`: a bounds-check panic would be an
//! unwind out of a signal frame, which is a worse outcome than answering
//! `None`, and `None` is always a legal answer.
//!
//! # Hazard 2 — a JIT code buffer is freed and its address REUSED
//!
//! This is the one that makes the feature hard, and the reason it did not land
//! with the rest of the round. `CompiledMethod::drop` unmaps the buffer it
//! owns, and — in that function's own words — "the address is then reusable by
//! the next `alloc_executable`". An entry left behind would eventually match a
//! PC belonging to *different* code, and the handler would resume execution at
//! a stale recovery address inside a live method. That is not a crash; it is
//! silent, arbitrary control flow.
//!
//! Two things close it:
//!
//! * `CompiledMethod::drop` calls [`unregister_range`] for its own code range,
//!   beside the `unregister_jit_method_name` call that exists for exactly the
//!   same reason.
//! * **A retired slot is never re-registered into.** Retiring stores a
//!   `TOMBSTONE` into the slot's `fault_pc`; nothing ever puts a live key back.
//!   Re-registration into a slot would open a genuine race — a reader that has
//!   already matched `fault_pc` could read a `recover_pc` that a concurrent
//!   re-registration has since overwritten, and jump into the wrong method.
//!   Refusing to re-register makes that unrepresentable rather than merely
//!   unlikely.
//!
//! ## A tombstone is not slot reuse, and the distinction is the whole argument
//!
//! The previous shape of this table said "slots are never reused" and meant it
//! literally: a monotonic cursor, a slot retired by storing `0`, and the `0`
//! never written over. The tombstone keeps the property that *mattered* and
//! drops the one that was incidental.
//!
//! What mattered: a slot that has ever held a live key never holds a
//! **different** live key afterwards. A reader that matched `fault_pc` at slot
//! `i` is therefore guaranteed that `RECOVER_PC[i]` is still the value that was
//! published with that key — nobody is allowed to have overwritten it. That is
//! exactly the read the signal handler performs, and it is still safe here for
//! the same reason it was safe before: the only transition out of "live" is
//! live → `TOMBSTONE`, and `TOMBSTONE` matches no key.
//!
//! What was incidental: allocating slots in registration order. That is what
//! cost O(n) per lookup, and it is what the hash table replaces.
//!
//! A per-slot generation counter that [`recover`] re-validated after reading
//! `recover_pc` would permit real reuse, and it was considered. It is not here,
//! because it buys capacity this workload does not need at the price of the one
//! read in the codebase that must be provably simple. Tombstones already bound
//! the table the way the old design did, and exhaustion already has a defined,
//! safe behaviour (see below). If the decline count ever becomes non-trivial in
//! a real run, generation counters are the upgrade — and the argument for them
//! has to be written out here first, including what a reader does when it
//! observes the generation move between the key match and the `recover_pc`
//! load. (It would have to answer `None` and let the crash reporter have the
//! fault, because a moved generation means it cannot prove whose recovery
//! address it just read.)
//!
//! ## Why the old table was O(n²) over a run, and this one is not
//!
//! The old [`recover`] was a linear scan of up to 32,768 atomics *inside the
//! signal handler*, and the old `unregister_range` was the same scan on **every**
//! `CompiledMethod::drop`. Because slots were never recycled, the high-water
//! mark only rose: under sustained recompilation the scanned prefix grew toward
//! capacity, every recovered NPE walked the whole array, and the total work over
//! a run was quadratic in the number of sites ever compiled. The module doc
//! anticipated *exhaustion* and handled it; it did not anticipate the cost of
//! approaching exhaustion.
//!
//! The table is now open-addressed on `fault_pc` with linear probing, so
//! [`recover`] is O(1) expected and bounded by `MAX_PROBE` in the worst case.
//! The one rule a tombstone imposes on probing is the rule that is easiest to
//! get wrong: **a probe must not stop at a tombstone, only at a genuinely empty
//! slot.** Stopping at a tombstone would lose every key that had ever probed
//! past it, silently, which in this module means losing NPEs to the crash
//! reporter.
//!
//! ## Publication order, unchanged in substance
//!
//! Insertion is two-phase so that the store of `recover_pc` still happens before
//! the key is visible. A slot is claimed by a CAS from `EMPTY` to `RESERVED`,
//! then `recover_pc` is stored, then `fault_pc` is stored with `Release`. A
//! reader that loads a matching `fault_pc` with `Acquire` therefore always sees
//! the paired `recover_pc`, which is the same argument the two-array version
//! made. `RESERVED` is a third non-key state: a reader that meets it neither
//! matches it nor stops at it, and a concurrent inserter that meets it cannot
//! claim it, so an in-flight registration is invisible rather than half-visible.
//!
//! ## Retirement is sub-linear, via a per-page chain
//!
//! `unregister_range` no longer sweeps the table. Every successful registration
//! also pushes its slot index onto a lock-free chain selected by
//! `fault_pc >> PAGE_SHIFT` — a per-4-KiB-page bucket, which is the granularity
//! `alloc_executable` hands out code in. Retiring `[base, base + len)` walks the
//! chains of the pages that range covers and nothing else, so a method drop
//! costs roughly the number of sites in that method rather than the capacity of
//! the table.
//!
//! Two honest notes on that claim:
//!
//! * Dead chain nodes ARE spliced out as of 2026-09-16, under a striped
//!   mutex the lookup path never takes (see `CHAIN_STRIPES`). This paragraph
//!   used to record the opposite as a limit — "chain nodes are never unlinked,
//!   so a repeatedly-recompiled page accumulates dead nodes, bounded by `CAP`,
//!   degenerating to the old sweep cost in the worst case" — and that limit
//!   was the reason retirement could still become linear on exactly the
//!   workload the chains were built for. `unlinked_chain_nodes()` counts the
//!   splices; a rising `retired` beside a flat `unlinked` is the signature of
//!   it having stopped.
//! * A range spanning more than `MAX_RANGE_PAGES` pages falls back to a full
//!   sweep, because walking that many buckets would cost more than the sweep.
//!   No JIT code buffer is anywhere near 4 MiB, so this is a guard against a
//!   caller mistake rather than a path the VM takes.
//!
//! The chain push happens *after* the key is published, so a retirement that
//! raced a registration of the same range could miss the new site. No such race
//! exists: registration for a method's range completes before the method is
//! published, and the drop that retires the range happens only when no frame of
//! it is live.
//!
//! ## The invariant that arrives through the door nobody checks
//!
//! [`unregister_range`] and [`register`] agree on the base address only because
//! **the method entry IS the buffer base**. `CompiledMethod::drop` retires
//! `[entry, entry + buffer.pos())` while registration keys sites off
//! `cm.entry + fault_off`; `x64::driver` establishes the equality in as many
//! words (`let entry_offset = 0; // prologue starts at offset 0`), and the
//! OSR-trampoline purge in that same `Drop` already leans on it. Give the
//! prologue a non-zero offset and every site below the new entry silently stops
//! being retired — which is exactly the stale-entry hazard this design exists
//! to prevent, arriving through the one door nobody would think to check.
//!
//! That used to be prose only. It now has two enforcers that live in this file:
//! [`note_registration_base`], which a registering caller passes both addresses
//! to and which trips a `debug_assert` when they differ; and the
//! `the_method_entry_is_still_the_buffer_base` test below, which reads
//! `x64/driver.rs` with `include_str!` and fails if the `entry_offset`
//! declaration stops being a literal zero. The second one is the important one,
//! because the hazard is introduced by an edit, not by a run.
//!
//! Retirement cannot race a fault in the method being retired, and the reason
//! is an invariant this file borrows rather than establishes: a
//! `CompiledMethod` is only dropped when **no frame of it is live** — the same
//! sentence `release_compile_id` and `unregister_jit_method_name` rely on in
//! the very same `Drop`. A thread cannot be executing at a PC inside a method
//! that is being unmapped, so it cannot be faulting at one either. What the
//! retirement protects against is not that race but the LATER one: a fault in
//! whatever code `alloc_executable` puts at the same address next.
//!
//! ## Exhaustion: a load factor, and declining rather than spinning
//!
//! Because a tombstone occupies its slot forever, "occupied" only rises, and a
//! long run with heavy recompilation still fills the table. An open-addressed
//! table degrades badly as it approaches full — probe runs lengthen toward the
//! capacity — so it is capped at a **3/4 load factor** on live-plus-tombstoned
//! entries: 24,576 of the 32,768 slots may ever be claimed. Past that,
//! [`register`] returns `false` without probing at all. A probe run that
//! exceeds `MAX_PROBE` before finding an empty slot declines the same way,
//! which is the second, hash-quality-dependent full condition. Neither path
//! spins and neither path blocks.
//!
//! A decline is NOT a fallback to an explicit check, and this paragraph used to
//! say it was. Registration happens after the artifact is emitted, so the
//! declined site's load already has no `TEST`/`JZ` in front of it: a null
//! receiver there is a process-killing fault, not an NPE. `DECLINED` counts it
//! and [`counts`] reports it, but counting is not containment. What contains
//! it is [`active`]'s headroom gate (hazard 5): elision stops well before the
//! load cap, so the feature turns itself off — every NEW compile takes
//! explicit checks — while there is still room for the compiles already in
//! flight. The remaining corner (one compile outrunning the headroom) is closed
//! by the driver: an artifact with a declined site is refused, not installed
//! (`x64::driver::register_implicit_null_sites`); see hazard 5.
//!
//! # Hazard 3 — recovering from a fault that is NOT an implicit null check
//!
//! A genuine JIT bug also produces a `SIGSEGV` inside compiled code, and
//! silently resuming from one would convert a diagnosable crash into corrupted
//! state. Every one of these must hold before [`recover`] answers:
//!
//! * the signal is a memory-access fault (checked by the caller);
//! * `si_code` says a real hardware fault, so `si_addr` is an address at all
//!   rather than a union member left over from a `kill -SEGV`;
//! * the faulting address is inside the **null page** (`< 4096`) — a null
//!   receiver plus a small field offset, and nothing else;
//! * the faulting PC is an **exact** registered entry, not merely inside some
//!   compiled method's range.
//!
//! The last two are what separate "a null receiver reached a load we chose not
//! to guard" from "compiled code dereferenced garbage". A wild pointer does
//! not land in the first page, and a PC that is not the exact instruction we
//! registered is not ours.
//!
//! # Hazard 4 — nothing is installed to recover the fault
//!
//! An elided check is only half of the mechanism: the other half is a
//! process-wide fault handler (a Windows vectored exception handler, or the
//! Unix `SIGSEGV` action) that calls [`recover`]. The VM installs one from
//! `install_hardware_fault_handler`, which `vm-cli` and `libcratonvm` call at
//! startup. An embedding that never calls it — every `vm/tests` integration
//! binary, where the test-harness shim that installs it is `cfg(test)`-only —
//! used to get elided checks all the same. A null receiver then faulted with
//! no handler at all, and a Java `NullPointerException` killed the process
//! with `STATUS_ACCESS_VIOLATION` and no report
//! (`implicit-null-checks-crash-a-process-with-no-fault-handler-FIXED-20260912.md`).
//!
//! So elision is gated on [`active`], not on [`enabled`] alone: the handler's
//! installer calls [`note_fault_handler_installed`], and until it has, every
//! site keeps its explicit check.
//!
//! # What the compiler must guarantee, and does
//!
//! The registered PC has to be an instruction that (a) faults whenever the
//! receiver is null, and (b) has no side effect before it faults. The
//! compiler verifies the emitted bytes rather than trusting the source: see
//! `x64::Compiler::bind_implicit_null_recovery`, which decodes the instruction
//! at the recorded offset and **fails the compile** if it is not the expected
//! `MOV r32, [RAX + disp32]` with `disp32 < 4096`. A failed compile falls back
//! to the interpreter, so the fail-closed direction never leaves unguarded
//! code running.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// Faults at or above this address are never an implicit null check. One page
/// covers a null receiver plus any field offset the compiler will fold into
/// the addressing mode; the emitter's own verification caps the displacement
/// at the same number, so the two agree by construction.
pub const NULL_PAGE_LIMIT: usize = 4096;

/// Table **capacity**, in slots. It used to be a high-water bound on a linear
/// array; it is now the size of the open-addressed table, and it must be a
/// power of two because the probe index is masked rather than divided.
///
/// 32,768 slots is 256 KiB of `fault_pc`, 256 KiB of `recover_pc`, 128 KiB of
/// chain links and 32 KiB of bucket heads — 672 KiB of zero-initialised BSS,
/// up from the 512 KiB the two-array version used. The extra 160 KiB is what
/// buys sub-linear retirement; it is untouched pages until the JIT registers
/// its first site.
const CAP: usize = 1 << 15;

/// The most slots of a `cap`-slot table that may ever be claimed: **3/4**,
/// counting tombstones, because a tombstone costs a probe exactly as much as a
/// live entry does. For the production table that is 24,576 of 32,768.
///
/// At 3/4 load the expected probe run for a successful lookup is under three
/// slots and for an unsuccessful one under nine; letting the table run to 100%
/// would turn both into the linear scan this rewrite exists to delete.
const fn load_cap_of(cap: usize) -> usize {
    cap - cap / 4
}

/// Slots below the load cap at which [`active`] stops licensing new implicit
/// sites: one eighth of capacity (4,096 of 32,768). That is a bound on the
/// registrations of compiles already past their elision decision when the gate
/// closes — several compile threads' worth of the largest methods seen — and
/// it leaves the table's last stretch for exactly those.
const fn headroom_of(cap: usize) -> usize {
    cap / 8
}

/// Probe-run bound, for both insertion and lookup. They must use the same
/// number: [`Table::insert`] only ever places a key within this many steps of
/// its hash, so [`Table::lookup`] searching the same window can never miss one.
///
/// A run this long at 3/4 load is a hash-quality failure rather than a capacity
/// failure, and it is treated as fullness — decline, count it, move on.
const MAX_PROBE: usize = 64;

/// Code-page granularity for the retirement chains. `alloc_executable` hands
/// out page-granular buffers, so a method's sites live in a small run of pages.
const PAGE_SHIFT: u32 = 12;

/// Number of retirement chain heads. A power of two; the bucket index is the
/// mixed page number masked down. A quarter of `CAP`, so a table full of
/// entries on distinct pages averages four to a chain.
const BUCKETS: usize = CAP >> 2;

/// Stripes of the chain-mutation lock. A power of two; a bucket takes
/// `bucket & (CHAIN_STRIPES - 1)`.
///
/// # Why a lock is admissible here, when the table itself may not have one
///
/// The signal handler's path is [`recover`] -> [`Table::lookup`], and that
/// touches **only** the hash table: `fault_pc`, `recover_pc` and one counter,
/// all atomics, no lock. The retirement chains are walked by exactly two
/// callers, both on ordinary threads — [`register`] (the compiler, at compile
/// time) and [`unregister_range`] (`CompiledMethod::drop`, on a mutator). So
/// the chains can be mutated under a lock without putting one anywhere near a
/// fault, and that is what lets a dead node be UNLINKED rather than merely
/// tombstoned.
///
/// Striped rather than one global lock because `register` is per JIT site and
/// a single lock would serialise every compile thread's registrations behind
/// every other's. 64 stripes over 8,192 buckets: two buckets sharing a stripe
/// only ever serialise with each other, and the critical section is a handful
/// of relaxed atomic ops with no allocation and no call out.
const CHAIN_STRIPES: usize = 64;

/// Beyond this many pages, [`unregister_range`] sweeps instead of walking
/// buckets. 1,024 pages is 4 MiB — an order of magnitude above the largest JIT
/// buffer, so this is a guard against a caller mistake, not a path the VM uses.
const MAX_RANGE_PAGES: usize = 1024;

/// A slot that has never been claimed. A real faulting PC is never 0, and
/// [`register`] refuses 0 anyway.
const EMPTY: usize = 0;

/// A slot claimed but not yet published. Never matches a key and never stops a
/// probe. `usize::MAX` is not a canonical code address on any target this
/// backend emits for, and [`register`] refuses it explicitly regardless.
const RESERVED: usize = usize::MAX;

/// A slot whose entry was retired. Never matches a key, never stops a probe,
/// and — this is the hazard-2 property — is never claimed again.
const TOMBSTONE: usize = usize::MAX - 1;

/// The key half of the table. See [`EMPTY`], [`RESERVED`], [`TOMBSTONE`].
///
/// The inline `const` block is a fresh `AtomicUsize` PER ELEMENT. It replaced a
/// named `const ZERO`, which worked for the same reason -- a `const` is copied
/// at each use -- but is the shape `clippy::declare_interior_mutable_const`
/// warns about, because the same name read anywhere else would silently
/// produce a temporary to mutate rather than shared state.
static FAULT_PC: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
/// The value half, published *before* the key it belongs to.
static RECOVER_PC: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
/// `slot index + 1` of the next entry on the same page chain; 0 ends the chain.
/// `u32` because an index is bounded by [`CAP`], and halving this array is
/// worth one cast.
static CHAIN_NEXT: [AtomicU32; CAP] = [const { AtomicU32::new(0) }; CAP];
/// Per-page chain heads, `slot index + 1`; 0 means the page has no entries.
static BUCKET_HEAD: [AtomicU32; BUCKETS] = [const { AtomicU32::new(0) }; BUCKETS];
/// Guards mutation of [`BUCKET_HEAD`] / [`CHAIN_NEXT`] only. See
/// [`CHAIN_STRIPES`] for why a lock is admissible on this path and not on the
/// lookup path.
static CHAIN_LOCK: [parking_lot::Mutex<()>; CHAIN_STRIPES] =
    [const { parking_lot::Mutex::new(()) }; CHAIN_STRIPES];
/// Slots ever claimed, live or tombstoned. Compared against `load_cap_of(CAP)`.
///
/// This currently equals the `registered` counter exactly, because every claim
/// is a successful registration and no claim is ever released. It is a separate
/// static anyway: the counters are a reporting surface that may grow or change
/// meaning, and the load factor is a correctness bound that must not.
static OCCUPIED: AtomicUsize = AtomicUsize::new(0);

/// `[registered, retired, recovered, declined]`, indexed by the `C_*` consts.
/// One array rather than four statics: they are read together by [`counts`].
static COUNTERS: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];
const C_REGISTERED: usize = 0;
const C_RETIRED: usize = 1;
const C_RECOVERED: usize = 2;
const C_DECLINED: usize = 3;
/// Dead chain nodes spliced out of a retirement bucket. See
/// [`Table::retire_bucket`] — a rising `retired` with a flat `unlinked` would
/// mean the splice had stopped working and the chains were growing again.
const C_UNLINKED: usize = 4;
/// Set once a fault handler that calls [`recover`] is installed. See hazard 4.
static FAULT_HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);
/// Times [`note_registration_base`] was handed an entry that was not the buffer
/// base. Non-zero means the prose invariant above has been broken at runtime.
static BASE_MISMATCHES: AtomicUsize = AtomicUsize::new(0);
/// Live entries [`Table::insert`] found at a PC it was registering, with a
/// DIFFERENT recovery address, and retired: sites of a freed buffer that were
/// never retired (hazard 2). Non-zero means some drop path skipped
/// [`unregister_range`]; the insert repairs the table, this makes it visible.
static STALE_DUPLICATES_RETIRED: AtomicUsize = AtomicUsize::new(0);

/// See [`STALE_DUPLICATES_RETIRED`]. Should read zero forever.
pub fn stale_duplicates_retired() -> usize {
    STALE_DUPLICATES_RETIRED.load(Ordering::Relaxed)
}

/// Is `pc` usable as a table key? The three non-key states are excluded, which
/// is why [`recover`] can compare a caller-supplied PC against a slot without
/// ever colliding with a sentinel.
fn is_key(pc: usize) -> bool {
    pc != EMPTY && pc != RESERVED && pc != TOMBSTONE
}

/// A 64-bit avalanche (the MurmurHash3 finaliser). Code addresses cluster
/// hard — same allocator, page-aligned, 16-byte-ish instruction spacing — so
/// the low bits of a PC are close to useless as a slot index on their own.
/// Mixing first is what makes "O(1) expected" an honest claim rather than a
/// hope about address layout.
const fn mix(x: usize) -> usize {
    let mut z = x as u64;
    z ^= z >> 33;
    z = z.wrapping_mul(0xff51_afd7_ed55_8ccd);
    z ^= z >> 33;
    z = z.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    z ^= z >> 33;
    z as usize
}

/// A borrowed view of one table's arrays.
///
/// The production table is the `static` arrays above and nothing else is
/// allocated to reach them — `static_table()` builds four slice references and
/// two plain references, which is free and, crucially, allocation-free in a
/// signal handler. The indirection exists so the tests can stand up a *small*
/// table with its own counters: filling the process-global table to prove the
/// load factor declines would exhaust it for every other test in the binary,
/// and forcing hash collisions in a 32,768-slot table costs a search the tests
/// should not have to run.
struct Table<'a> {
    fault_pc: &'a [AtomicUsize],
    recover_pc: &'a [AtomicUsize],
    chain_next: &'a [AtomicU32],
    bucket_head: &'a [AtomicU32],
    /// Striped guard for chain mutation — see [`CHAIN_STRIPES`]. Never taken
    /// by [`Table::lookup`], which is the signal-handler path.
    chain_lock: &'a [parking_lot::Mutex<()>],
    occupied: &'a AtomicUsize,
    counters: &'a [AtomicUsize; 5],
}

fn static_table() -> Table<'static> {
    Table {
        fault_pc: &FAULT_PC,
        recover_pc: &RECOVER_PC,
        chain_next: &CHAIN_NEXT,
        bucket_head: &BUCKET_HEAD,
        chain_lock: &CHAIN_LOCK,
        occupied: &OCCUPIED,
        counters: &COUNTERS,
    }
}

impl Table<'_> {
    fn cap(&self) -> usize {
        self.fault_pc.len()
    }

    /// Index mask. Valid only because every constructor uses a power of two;
    /// the `debug_assert` is the only place that is enforced.
    fn mask(&self) -> usize {
        debug_assert!(self.cap().is_power_of_two(), "capacity must be 2^k");
        self.cap() - 1
    }

    /// Slots that may ever be claimed, live plus tombstoned: 3/4 of capacity.
    fn load_cap(&self) -> usize {
        load_cap_of(self.cap())
    }

    /// Is there room below the load cap for the registrations of compiles
    /// that are deciding to elide checks right now? See hazard 5 on
    /// [`active`]: elision stops [`headroom_of`] slots early so that those
    /// in-flight compiles do not decline after their code is emitted.
    fn has_headroom(&self) -> bool {
        self.occupied.load(Ordering::Relaxed)
            < self.load_cap().saturating_sub(headroom_of(self.cap()))
    }

    /// Probe budget, clamped so a table smaller than [`MAX_PROBE`] cannot wrap
    /// around and visit a slot twice.
    fn budget(&self) -> usize {
        MAX_PROBE.min(self.cap())
    }

    fn bucket_of(&self, pc: usize) -> usize {
        debug_assert!(self.bucket_head.len().is_power_of_two());
        mix(pc >> PAGE_SHIFT) & (self.bucket_head.len() - 1)
    }

    /// Insert one site, or decline. `false` means the site has no recovery
    /// entry — and since registration runs after emission, the caller must not
    /// run code whose load at `fault_pc` was left unchecked (hazard 5 on
    /// [`active`]).
    ///
    /// Assumes [`is_key`] holds for `fault_pc` and `recover_pc != 0`; the public
    /// [`register`] is what establishes that.
    fn insert(&self, fault_pc: usize, recover_pc: usize) -> bool {
        // Reserve capacity BEFORE probing, so that concurrent registrations
        // cannot between them push the table past its load factor. A probe
        // that then fails gives the reservation back — the counter is a bound,
        // not a log, and `OCCUPIED` must end up equal to the number of slots
        // actually claimed or the bound drifts.
        if self.occupied.fetch_add(1, Ordering::Relaxed) >= self.load_cap() {
            self.occupied.fetch_sub(1, Ordering::Relaxed);
            self.counters[C_DECLINED].fetch_add(1, Ordering::Relaxed);
            return false;
        }

        let mask = self.mask();
        let mut i = mix(fault_pc) & mask;
        for _ in 0..self.budget() {
            let slot = &self.fault_pc[i];
            // `Acquire`, so that a live key observed here comes with the
            // `recover_pc` published before it — the duplicate arm below reads
            // that value to tell a repeated registration from a stale one.
            let cur = slot.load(Ordering::Acquire);
            if cur == EMPTY {
                // Claim in two phases. Between the CAS and the `Release` store
                // the slot reads `RESERVED`, which no reader matches and no
                // probe stops at, so `recover_pc` is always in place before any
                // reader can be looking at the key that points to it.
                if slot
                    .compare_exchange(EMPTY, RESERVED, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    self.recover_pc[i].store(recover_pc, Ordering::Relaxed);
                    slot.store(fault_pc, Ordering::Release);
                    self.link(i, fault_pc);
                    self.counters[C_REGISTERED].fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                // Lost the race for this slot. Re-read it rather than stepping
                // on: the winner's key might be ours (the duplicate case), and
                // in any event the slot is no longer empty. This costs one
                // probe from the budget, which is the honest accounting.
                continue;
            }
            if cur == fault_pc {
                // A *live* duplicate key.
                //
                // Same recovery address: the identical mapping is already
                // published (a repeated registration of one artifact). There
                // is nothing to change, and touching the slot would open a
                // window in which a fault at this PC finds no entry. Accept.
                let existing = self.recover_pc[i].load(Ordering::Relaxed);
                if existing == recover_pc {
                    self.occupied.fetch_sub(1, Ordering::Relaxed);
                    return true;
                }
                // Different recovery address: STALE. Every legitimate path
                // retires a range before its addresses are handed out again,
                // so this entry belongs to a buffer that was freed without
                // retiring its sites — the hazard-2 symptom itself — and the
                // PC now lies inside the buffer being registered, which is
                // not yet published, so no thread can be executing at it and
                // no reader can legitimately want the old answer.
                //
                // This used to DECLINE, arguing "the new site keeps its
                // explicit check, and the stale entry is left exactly as
                // exposed as it already was — not worse". Both halves were
                // wrong. The new site has no explicit check (registration
                // runs after emission; see hazard 5 on [`active`]), so a null
                // receiver at this PC faults — and the lookup then MATCHES
                // the stale entry and resumes at the dead buffer's recovery
                // address inside this method's code. That is the silent
                // arbitrary jump hazard 2 exists to prevent, delivered by the
                // decline. Retire the stale entry (live -> TOMBSTONE, the one
                // transition a slot may make) and keep probing for a fresh
                // slot, exactly as if the stale key had been retired on time.
                if slot
                    .compare_exchange(cur, TOMBSTONE, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    self.counters[C_RETIRED].fetch_add(1, Ordering::Relaxed);
                    STALE_DUPLICATES_RETIRED.fetch_add(1, Ordering::Relaxed);
                }
                i = (i + 1) & mask;
                continue;
            }
            // RESERVED, TOMBSTONE, or somebody else's key: step on. A tombstone
            // is emphatically NOT a free slot — reusing one is the race this
            // module exists to make unrepresentable.
            i = (i + 1) & mask;
        }

        // Ran out of probe budget without finding an empty slot. Same
        // fail-closed answer as a full table, and counted the same way.
        self.occupied.fetch_sub(1, Ordering::Relaxed);
        self.counters[C_DECLINED].fetch_add(1, Ordering::Relaxed);
        false
    }

    /// The stripe guarding bucket `b`'s chain. See [`CHAIN_STRIPES`].
    fn stripe(&self, b: usize) -> &parking_lot::Mutex<()> {
        debug_assert!(self.chain_lock.len().is_power_of_two());
        &self.chain_lock[b & (self.chain_lock.len() - 1)]
    }

    /// Push slot `i` onto its page's retirement chain.
    ///
    /// Acyclic by construction: a slot is claimed exactly once and never
    /// re-registered into, so it is pushed exactly once, and its `next` is
    /// fixed before it becomes reachable.
    ///
    /// # This was a lock-free CAS push until 2026-09-16
    ///
    /// It was correct, and it was also the reason a dead node could only ever
    /// be TOMBSTONED and never UNLINKED: removing a node from a lock-free
    /// singly-linked list needs the predecessor's `next` to be updated while no
    /// reader is standing on it, which without hazard pointers or epochs is
    /// not something a `compare_exchange` can give you. The consequence was
    /// documented as a limit — "a repeatedly-recompiled page accumulates dead
    /// nodes, bounded by `CAP`, degenerating to the old sweep cost in the worst
    /// case".
    ///
    /// The lock removes the limit rather than the documentation, and it costs
    /// nothing where it matters: the *lookup* path — the one inside the fault
    /// handler — does not touch the chains and takes no lock. See
    /// [`CHAIN_STRIPES`] for that argument in full.
    fn link(&self, i: usize, fault_pc: usize) {
        let b = self.bucket_of(fault_pc);
        let node = (i as u32) + 1;
        let _guard = self.stripe(b).lock();
        let head = self.bucket_head[b].load(Ordering::Relaxed);
        self.chain_next[i].store(head, Ordering::Relaxed);
        self.bucket_head[b].store(node, Ordering::Release);
    }

    /// The recovery PC for `key`, or `None`.
    ///
    /// Async-signal-safe: acquire/relaxed loads and one lock-free `fetch_add`.
    /// Every index goes through `get`, so a capacity bug answers `None` instead
    /// of panicking out of a signal frame.
    fn lookup(&self, key: usize) -> Option<usize> {
        let mask = self.mask();
        let mut i = mix(key) & mask;
        for _ in 0..self.budget() {
            let pc = self.fault_pc.get(i)?.load(Ordering::Acquire);
            if pc == EMPTY {
                // The ONLY state that ends a probe. `insert` places a key at
                // the first empty slot in this same walk, and no slot ever
                // returns to empty, so nothing we are looking for can be
                // beyond here. Tombstones and reservations fall through on
                // purpose: stopping at one would silently lose every key that
                // had probed past it.
                return None;
            }
            if pc == key {
                // The key was published with `Release` after `recover_pc` was
                // stored, and this load was `Acquire`, so the value below is
                // the one that was published with this key. Nothing may have
                // overwritten it: the slot's only remaining transition is to
                // TOMBSTONE, which would have failed the comparison above.
                let target = self.recover_pc.get(i)?.load(Ordering::Relaxed);
                if target != 0 {
                    self.counters[C_RECOVERED].fetch_add(1, Ordering::Relaxed);
                    return Some(target);
                }
                // Unreachable in practice — `register` refuses a zero
                // `recover_pc`, so a published key always has a non-zero
                // partner. Kept as the same belt-and-braces the array version
                // carried, and it continues the probe rather than answering,
                // which is what the array version did too.
            }
            i = (i + 1) & mask;
        }
        None
    }

    /// Retire every live entry whose key lies in `[base, end)`.
    fn retire_range(&self, base: usize, end: usize) {
        if end <= base {
            return;
        }
        let first_page = base >> PAGE_SHIFT;
        let last_page = (end - 1) >> PAGE_SHIFT;
        let pages = last_page - first_page + 1;
        if pages > MAX_RANGE_PAGES {
            self.retire_by_sweep(base, end);
            return;
        }
        for page in first_page..=last_page {
            self.retire_bucket(self.bucket_of(page << PAGE_SHIFT), base, end);
        }
    }

    /// Retire every entry of bucket `b` that lies in `[base, end)`, and
    /// **unlink every dead node the walk passes** — including ones an earlier
    /// call tombstoned, and ones `retire_by_sweep` tombstoned without walking a
    /// chain at all.
    ///
    /// Unlinking is what keeps this sub-linear over a run. Without it a page
    /// that is compiled, dropped and recompiled repeatedly grows a chain of
    /// tombstoned nodes that every later retirement of that page walks in full
    /// — the chain is bounded only by `CAP`, so the worst case is the whole
    /// table, which is the linear sweep the chains replaced.
    ///
    /// Held under the bucket's stripe (see [`CHAIN_STRIPES`]), which is the
    /// only thing that makes the predecessor update safe. The lookup path takes
    /// no lock and is unaffected: it never reads a chain.
    fn retire_bucket(&self, b: usize, base: usize, end: usize) {
        let _guard = self.stripe(b).lock();
        let mut prev: Option<usize> = None;
        let mut cur = self.bucket_head[b].load(Ordering::Acquire);
        // Bounded by the table: a chain cannot be longer than the number of
        // slots, and a cycle would otherwise hang a mutator inside
        // `CompiledMethod::drop`. `link` makes a cycle unrepresentable, so this
        // bound is a backstop and not a policy.
        let mut steps = 0usize;
        while cur != 0 && steps <= self.cap() {
            steps += 1;
            let i = (cur - 1) as usize;
            let next = self.chain_next[i].load(Ordering::Acquire);
            self.retire_slot(i, base, end);
            let dead = self
                .fault_pc
                .get(i)
                .map(|s| s.load(Ordering::Acquire) == TOMBSTONE)
                .unwrap_or(false);
            if dead {
                // Splice it out. `prev` deliberately does NOT advance: the new
                // predecessor of the next node is still the node before this
                // one.
                match prev {
                    None => self.bucket_head[b].store(next, Ordering::Release),
                    Some(p) => self.chain_next[p].store(next, Ordering::Release),
                }
                self.counters[C_UNLINKED].fetch_add(1, Ordering::Relaxed);
            } else {
                prev = Some(i);
            }
            cur = next;
        }
    }

    /// The fallback for absurdly large ranges: what the old `unregister_range`
    /// did on every call.
    fn retire_by_sweep(&self, base: usize, end: usize) {
        for i in 0..self.cap() {
            self.retire_slot(i, base, end);
        }
    }

    /// Tombstone slot `i` if it holds a live key inside `[base, end)`.
    ///
    /// A `compare_exchange` on the exact value observed, not a `swap`: a `swap`
    /// would happily overwrite a `RESERVED` slot and destroy an in-flight
    /// registration's key, leaving a published `recover_pc` with no way to
    /// reach it and a claimed slot that never becomes findable.
    ///
    /// `recover_pc` is deliberately left alone. A reader reaches it only after
    /// matching the key, and a `TOMBSTONE` matches nothing, so the stale value
    /// is unreachable rather than dangerous — and not clearing it keeps this to
    /// one store on the path `CompiledMethod::drop` takes for every method.
    fn retire_slot(&self, i: usize, base: usize, end: usize) {
        let slot = &self.fault_pc[i];
        let pc = slot.load(Ordering::Relaxed);
        if !is_key(pc) || pc < base || pc >= end {
            return;
        }
        if slot
            .compare_exchange(pc, TOMBSTONE, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            self.counters[C_RETIRED].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Is the implicit null check enabled? **Default ON** since 2026-09-02; opt
/// out with `CRATONVM_JIT_IMPLICIT_NULL_CHECK=0`.
///
/// # What the off arm restores, exactly
///
/// `emit_trusted_oop_receiver_check` at both `getfield` arms, unconditionally
/// — the behaviour of every binary before this feature existed. Nothing
/// registers, so every slot [`recover`] can probe is still `EMPTY` and it
/// answers `None` on its first load, and a fault in compiled code reaches the
/// crash reporter exactly as it always did. There is no degraded middle state.
///
/// # Read this before deciding the flag is unnecessary
///
/// This switch guards the only mechanism in this backend whose wrong arm is
/// **silent**. Every other one produces a wrong answer, which a test catches;
/// a stale or mis-shaped entry here resumes execution at an address the table
/// chose, which nothing catches. That is why the kill switch exists and why it
/// should keep existing even though the default moved: the first thing anyone
/// debugging an unexplained crash in compiled code should be able to do is
/// take this out of the picture in one run, on the same binary.
///
/// It was default-off through its soak (see `docs/JIT_OPTIMIZATION.md`): about
/// an hour of continuous execution plus two full regression-suite passes, with
/// 11,663 of 11,719 null dereferences recovered as translated hardware faults
/// under GC pressure, every checksum matching HotSpot, and 87/87 twice.
pub fn enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IMPLICIT_NULL_CHECK").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Record that a process-wide fault handler which calls [`recover`] is
/// installed. Called by the VM's hardware-fault installers; never reset,
/// because neither installer ever removes its handler.
pub fn note_fault_handler_installed() {
    FAULT_HANDLER_INSTALLED.store(true, Ordering::Release);
}

/// Has [`note_fault_handler_installed`] been called in this process?
pub fn fault_handler_installed() -> bool {
    FAULT_HANDLER_INSTALLED.load(Ordering::Acquire)
}

/// May the compiler elide a check now? [`enabled`] AND a recovering fault
/// handler installed (hazard 4) AND room left in the table for the sites the
/// compile is about to register (hazard 5, below). A compile that runs before
/// the handler exists, or after the table has filled, emits explicit checks,
/// which is the behaviour of the off arm.
///
/// # Hazard 5 — a declined registration is NOT a safe fallback
///
/// The elision decision is made while the method is being EMITTED; the site is
/// registered only after the whole artifact exists (`x64::driver`, after
/// `CompiledMethod::new`). By then the load that was chosen to fault carries
/// no `TEST`/`JZ` in front of it. So when [`register`] declines, the method
/// runs anyway with a faulting load the handler cannot translate, and a null
/// receiver there kills the process instead of raising
/// `NullPointerException`. The module doc used to call declining "the safe
/// direction: the compiler emits the explicit check for that site" — it
/// cannot, the check was already left out.
///
/// And declining is not hypothetical over a long run. Tombstones never give a
/// slot back, so the table has a LIFETIME budget of `load_cap_of(CAP)`
/// (24,576) registrations; one H2 workload counted 1,718 implicit sites
/// (`x64::null_check_elim`'s per-arm census), and every recompile,
/// deopt-and-recompile and tier change registers its sites again.
///
/// This gate stops elision `HEADROOM` slots before the load cap, so the
/// compiles already in flight when the gate closes still find room: from that
/// point every new compile takes explicit checks, the feature has turned
/// itself off, and nothing faults unrecoverably. The headroom is a bound on
/// in-flight registrations, not a proof — a single compile with more implicit
/// sites than the headroom, racing the last few slots, can still decline — so
/// the driver ALSO refuses to install an artifact any of whose sites was
/// declined (`x64::driver::register_implicit_null_sites` retires what the
/// artifact registered and the compile bails with `implicit-null-declined`).
/// This gate is what makes that refusal a corner rather than the steady state
/// of a long-running server.
pub fn active() -> bool {
    enabled() && fault_handler_installed() && static_table().has_headroom()
}

/// Assert, at the one moment both addresses are in hand, that a method's entry
/// really is its buffer base. Returns `true` when they agree.
///
/// This is the runtime half of the invariant the module doc spells out:
/// [`register`] keys sites off `entry + fault_off`, `CompiledMethod::drop`
/// retires `[buffer_base, buffer_base + pos)`, and the two describe the same
/// interval **only** while `entry == buffer_base`. Move the entry and the sites
/// below it stop being retired, which is a stale entry pointing into a buffer
/// the allocator has since handed to someone else.
///
/// A caller that gets `false` must register **nothing** for that method AND
/// must not install it: the sites' explicit checks were already elided when
/// the code was emitted, so an installed artifact would turn every null
/// receiver at them into a crash. The driver refuses the compile
/// (`implicit-null-entry-not-base`). In debug builds
/// the `debug_assert` fires first, because a build that can reach this has a
/// bug an author should see rather than a condition a run should absorb.
///
/// The compile-time half is the `the_method_entry_is_still_the_buffer_base`
/// test below, and it is the one that matters: this hazard is introduced by an
/// edit to the prologue, and the test fails on the edit rather than on the
/// unlucky run that first reuses an address.
pub fn note_registration_base(entry: usize, buffer_base: usize) -> bool {
    if entry == buffer_base {
        return true;
    }
    BASE_MISMATCHES.fetch_add(1, Ordering::Relaxed);
    debug_assert_eq!(
        entry, buffer_base,
        "the method entry must BE the buffer base: `unregister_range` retires \
         [buffer_base, buffer_base + pos) while `register` keys sites off \
         entry + fault_off, so a non-zero prologue offset silently stops \
         retiring every site below the entry -- a stale recovery address in a \
         reused buffer, which is hazard 2 arriving through the one door nobody \
         would think to check"
    );
    false
}

/// Register one site. `false` means the table cannot take it — full, past its
/// load factor, or a probe run too long. It does NOT mean an explicit check
/// takes the site's place: the code is already emitted without one, so a
/// caller that gets `false` must not install that artifact (hazard 5 on
/// [`active`], which is also what keeps this from happening in the first
/// place).
///
/// Publication order matters and is preserved: the slot is claimed as
/// `RESERVED`, `recover_pc` is stored **before** `fault_pc`, and `fault_pc` is
/// stored with `Release`. A reader that loads a matching `fault_pc` with
/// `Acquire` therefore always sees the paired `recover_pc`.
pub fn register(fault_pc: usize, recover_pc: usize) -> bool {
    // A zero on either side is refused rather than stored: `0` is this table's
    // "never claimed" state. `usize::MAX` and `usize::MAX - 1` are refused for
    // the same reason — they are the in-flight and retired states — even though
    // no real code address is either.
    if !is_key(fault_pc) || recover_pc == 0 {
        return false;
    }
    static_table().insert(fault_pc, recover_pc)
}

/// Retire every site inside `[base, base + len)`.
///
/// Called from `CompiledMethod::drop` before the buffer is unmapped. After
/// this returns, no PC in that range can be recovered — which is the whole
/// point, because the address is about to belong to someone else.
///
/// Cost is the number of entries ever registered on the pages this range
/// covers, not the capacity of the table: registration files each slot on a
/// per-page chain and this walks only the chains it needs. A range wider than
/// `MAX_RANGE_PAGES` (4 MiB) falls back to a full sweep.
pub fn unregister_range(base: usize, len: usize) {
    static_table().retire_range(base, base.saturating_add(len));
}

/// The recovery PC for a fault, or `None` to let the crash reporter have it.
///
/// **Async-signal-safe**: atomic loads and one lock-free counter increment,
/// nothing else. Callers must already have established that this was a
/// hardware memory-access fault; this function enforces the null-page and
/// exact-PC conditions.
///
/// O(1) expected: one hash, then a linear probe that stops at the first empty
/// slot and in the worst case after `MAX_PROBE` (64) steps. It is not a scan of
/// the table, which is what it used to be.
pub fn recover(fault_pc: usize, fault_addr: usize) -> Option<usize> {
    // Unchanged guards: a zero PC is refused, and so is any address outside the
    // null page. `is_key` additionally refuses the two sentinel values, which a
    // faulting PC can never legitimately be but which would otherwise be able
    // to match a reserved or retired slot.
    if !is_key(fault_pc) || fault_addr >= NULL_PAGE_LIMIT {
        return None;
    }
    static_table().lookup(fault_pc)
}

/// Dead retirement-chain nodes spliced out since process start.
///
/// Separate from [`counts`] rather than widening its tuple, because that tuple
/// is a published four-element shape with callers outside this crate. Read it
/// beside `retired`: retirements climbing while this stays flat is the
/// signature of the splice having stopped, which is the state the chains were
/// in before 2026-09-16 and which degrades retirement back to a linear sweep.
pub fn unlinked_chain_nodes() -> usize {
    COUNTERS[C_UNLINKED].load(Ordering::Relaxed)
}

/// `(registered, retired, recovered, declined)`.
///
/// All four, never a subset. `registered` alone cannot distinguish "the
/// feature is off" from "it is on and no site qualified"; `recovered` alone
/// cannot distinguish "no null receiver occurred" from "the table never had
/// the entry"; and a non-zero `declined` is a site whose load was emitted
/// without its check and then could not be registered — see hazard 5 on
/// [`active`] for why that is a crash risk and not merely a lost optimisation.
pub fn counts() -> (usize, usize, usize, usize) {
    (
        COUNTERS[C_REGISTERED].load(Ordering::Relaxed),
        COUNTERS[C_RETIRED].load(Ordering::Relaxed),
        COUNTERS[C_RECOVERED].load(Ordering::Relaxed),
        COUNTERS[C_DECLINED].load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table with its own arrays, its own load factor and its own counters.
    ///
    /// The process-global statics are shared by every test in the binary, so a
    /// test that fills the table to prove the load factor would break every
    /// test that ran after it. Everything that needs to see the table *full*,
    /// or needs collisions without a search, uses one of these instead — and
    /// runs the same [`Table`] methods the public functions run, because the
    /// public functions are two-line wrappers over them.
    struct OwnedTable {
        fault_pc: Vec<AtomicUsize>,
        recover_pc: Vec<AtomicUsize>,
        chain_next: Vec<AtomicU32>,
        bucket_head: Vec<AtomicU32>,
        chain_lock: Vec<parking_lot::Mutex<()>>,
        occupied: AtomicUsize,
        counters: [AtomicUsize; 5],
    }

    impl OwnedTable {
        fn new(cap: usize, buckets: usize) -> Self {
            assert!(cap.is_power_of_two() && buckets.is_power_of_two());
            let atomics = |n: usize| (0..n).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();
            let atomics32 = |n: usize| (0..n).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
            Self {
                fault_pc: atomics(cap),
                recover_pc: atomics(cap),
                chain_next: atomics32(cap),
                bucket_head: atomics32(buckets),
                // One stripe per bucket in a test table: the striping exists to
                // stop compile threads serialising on each other, and a test
                // has one thread.
                chain_lock: (0..buckets)
                    .map(|_| parking_lot::Mutex::new(()))
                    .collect::<Vec<_>>(),
                occupied: AtomicUsize::new(0),
                counters: [
                    AtomicUsize::new(0),
                    AtomicUsize::new(0),
                    AtomicUsize::new(0),
                    AtomicUsize::new(0),
                    AtomicUsize::new(0),
                ],
            }
        }

        fn view(&self) -> Table<'_> {
            Table {
                fault_pc: &self.fault_pc,
                recover_pc: &self.recover_pc,
                chain_next: &self.chain_next,
                bucket_head: &self.bucket_head,
                chain_lock: &self.chain_lock,
                occupied: &self.occupied,
                counters: &self.counters,
            }
        }

        fn declined(&self) -> usize {
            self.counters[C_DECLINED].load(Ordering::Relaxed)
        }

        fn retired(&self) -> usize {
            self.counters[C_RETIRED].load(Ordering::Relaxed)
        }
    }

    /// `n` addresses that all hash to the same slot of a table of `cap` slots,
    /// starting at `start` and stepping by one instruction's worth.
    ///
    /// Collisions have to be constructed rather than assumed: the whole point
    /// of [`mix`] is that nearby code addresses do NOT collide, so a test that
    /// wants a probe run must go and find one.
    fn colliding(start: usize, cap: usize, n: usize) -> Vec<usize> {
        let mask = cap - 1;
        let want = mix(start) & mask;
        let mut out = vec![start];
        let mut pc = start;
        let limit = start + (cap * n * 4096).max(1 << 20);
        while out.len() < n {
            pc += 0x10;
            assert!(
                pc < limit,
                "no {n} colliding addresses within {} bytes of {start:#x} -- the \
                 hash changed and this helper's search budget has to change with it",
                limit - start
            );
            if mix(pc) & mask == want {
                out.push(pc);
            }
        }
        out
    }

    /// Hazard 4: the flag alone must not elide a check. `active` also needs a
    /// recovering fault handler, which only the VM's installers note.
    #[test]
    fn elision_needs_a_recovering_fault_handler_as_well_as_the_flag() {
        if !enabled() {
            assert!(
                !active(),
                "the kill switch turns elision off whatever else holds"
            );
            return;
        }
        // Process-wide: another test may already have stood in for the
        // installer, in which case only the second half can be checked.
        if !fault_handler_installed() {
            assert!(!active(), "no handler installed, so no elision");
        }
        note_fault_handler_installed();
        assert!(fault_handler_installed());
        assert!(active(), "flag on and a handler installed: elide");
    }

    /// A registered site round-trips, and only for a null-page address.
    #[test]
    fn a_registered_site_recovers_only_inside_the_null_page() {
        let fault = 0x4000_0000_1000usize;
        let recover = 0x4000_0000_2000usize;
        assert!(register(fault, recover));

        assert_eq!(recover_at(fault, 0), Some(recover));
        assert_eq!(recover_at(fault, 15), Some(recover));
        assert_eq!(
            recover_at(fault, NULL_PAGE_LIMIT),
            None,
            "an address outside the null page is a wild pointer, not a null \
             receiver -- resuming from one would convert a diagnosable crash \
             into arbitrary control flow"
        );
        assert_eq!(
            recover_at(fault + 1, 0),
            None,
            "the PC must match EXACTLY; being inside some compiled method is \
             not enough"
        );
        unregister_range(fault, 1);
    }

    /// Retiring a range makes its PCs unrecoverable — the property that keeps
    /// a freed-and-reused buffer from resuming at a stale address.
    #[test]
    fn a_retired_range_stops_recovering_and_its_slot_is_never_reused() {
        let base = 0x5000_0000_0000usize;
        assert!(register(base + 0x10, base + 0x80));
        assert!(register(base + 0x20, base + 0x90));
        assert_eq!(recover_at(base + 0x10, 0), Some(base + 0x80));

        let before = OCCUPIED.load(Ordering::Relaxed);
        unregister_range(base, 0x100);
        assert_eq!(recover_at(base + 0x10, 0), None);
        assert_eq!(recover_at(base + 0x20, 0), None);

        // The slots are TOMBSTONED, not recycled. Putting a live key back into
        // one would let a reader that already matched `fault_pc` read a
        // `recover_pc` a concurrent re-registration had overwritten, so a fresh
        // registration must claim a slot that has never been claimed before.
        assert!(register(base + 0x30, base + 0xA0));
        assert!(
            OCCUPIED.load(Ordering::Relaxed) > before,
            "registration after a retirement must claim a FRESH slot"
        );
        unregister_range(base, 0x100);
    }

    /// A zero on either side is refused rather than stored, because `0` is the
    /// sentinel this table uses for "never claimed".
    #[test]
    fn a_zero_pc_is_refused_because_zero_is_the_empty_sentinel() {
        assert!(!register(0, 0x1234));
        assert!(!register(0x1234, 0));
    }

    /// The two non-zero sentinels are refused too, and — the reason it matters
    /// — cannot be used as a lookup key to match a reserved or retired slot.
    #[test]
    fn the_sentinel_values_are_refused_as_keys() {
        assert!(!register(RESERVED, 0x1234));
        assert!(!register(TOMBSTONE, 0x1234));
        assert_eq!(recover(RESERVED, 0), None);
        assert_eq!(recover(TOMBSTONE, 0), None);
    }

    /// A long probe run: many sites that all hash to one slot, each of which
    /// must still find its OWN recovery address.
    ///
    /// This is the property the old linear array had for free and that linear
    /// probing has to earn. A bug in the probe walk — an index that does not
    /// advance, a comparison against the slot rather than the key — shows up
    /// here as one site recovering another site's target, which in production
    /// is a jump into the wrong method's slow path.
    #[test]
    fn colliding_sites_each_recover_their_own_target() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let keys = colliding(0x4100_0000_0000, 64, 12);
        for (n, &k) in keys.iter().enumerate() {
            assert!(v.insert(k, 0x7000_0000_0000 + n), "insert {n} of 12");
        }
        for (n, &k) in keys.iter().enumerate() {
            assert_eq!(
                v.lookup(k),
                Some(0x7000_0000_0000 + n),
                "site {n} recovered somebody else's address"
            );
        }
        assert_eq!(t.declined(), 0, "12 sites at 3/4 of 64 must all fit");
    }

    /// A retired slot is skipped by insertion, not recycled by it.
    ///
    /// Register A, retire A, then register a B chosen to hash to A's slot. B
    /// must land somewhere else, A must stay unrecoverable, and B must recover
    /// its own address — which is the whole tombstone argument in one test.
    #[test]
    fn a_tombstoned_slot_is_skipped_by_the_next_registration() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let keys = colliding(0x4200_0000_0000, 64, 2);
        let (a, b) = (keys[0], keys[1]);

        assert!(v.insert(a, 0x7100_0000_0000));
        let slot_a = mix(a) & (64 - 1);
        assert_eq!(v.fault_pc[slot_a].load(Ordering::Relaxed), a);

        v.retire_range(a, a + 1);
        assert_eq!(t.retired(), 1);
        assert_eq!(
            v.fault_pc[slot_a].load(Ordering::Relaxed),
            TOMBSTONE,
            "retirement must leave a TOMBSTONE, not an empty slot -- an empty \
             slot would both end probes early and be claimable again"
        );
        assert_eq!(v.lookup(a), None);

        assert!(v.insert(b, 0x7100_0000_1000));
        assert_eq!(
            v.fault_pc[slot_a].load(Ordering::Relaxed),
            TOMBSTONE,
            "the new registration must NOT have taken the retired slot"
        );
        assert_eq!(v.lookup(a), None, "a retired site stays retired");
        assert_eq!(v.lookup(b), Some(0x7100_0000_1000));
    }

    /// The length of bucket `b`'s retirement chain.
    fn chain_len(v: &Table<'_>, b: usize) -> usize {
        let mut n = 0usize;
        let mut cur = v.bucket_head[b].load(Ordering::Acquire);
        while cur != 0 && n <= v.cap() {
            n += 1;
            cur = v.chain_next[(cur - 1) as usize].load(Ordering::Acquire);
        }
        n
    }

    /// Retiring a page SPLICES its dead nodes out; it does not merely
    /// tombstone them.
    ///
    /// # The failure this pins
    ///
    /// Slots are never reused, so before the splice existed a page that was
    /// compiled, dropped and recompiled grew a chain of tombstoned nodes that
    /// every later retirement of that page walked in full. The chain is bounded
    /// only by `CAP`, so the worst case is the whole table — which is exactly
    /// the linear sweep the chains were introduced to replace. A run with heavy
    /// same-address recompilation would have degraded back to O(table) per
    /// `CompiledMethod::drop` with no counter saying so.
    #[test]
    fn retiring_a_page_unlinks_its_dead_nodes() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        // Three sites on one code page, so one bucket, then retire the page.
        let page = 0x4400_0000_0000usize;
        let sites = [page + 0x10, page + 0x40, page + 0x80];
        for (k, site) in sites.iter().enumerate() {
            assert!(v.insert(*site, 0x7300_0000_0000 + k));
        }
        let b = v.bucket_of(page);
        assert_eq!(chain_len(&v, b), 3, "all three are on one page's chain");

        v.retire_range(page, page + (1 << PAGE_SHIFT));
        assert_eq!(
            chain_len(&v, b),
            0,
            "every node was dead, so the chain is empty — not three tombstones"
        );
        assert_eq!(
            t.counters[C_UNLINKED].load(Ordering::Relaxed),
            3,
            "and the splice is counted, so a chain that starts growing again              is visible rather than silent"
        );
        for site in sites {
            assert_eq!(v.lookup(site), None);
        }
    }

    /// A live node between two dead ones survives the splice, and the chain
    /// stays connected through it.
    ///
    /// The predecessor bookkeeping is the whole content of the splice: after
    /// removing a node, the next node's predecessor is still the node BEFORE
    /// the removed one, so `prev` must not advance. Getting that wrong either
    /// drops the tail (losing live sites, which turns a recoverable NPE into a
    /// crash) or leaves a cycle.
    #[test]
    fn a_live_node_survives_a_splice_on_both_sides_of_it() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        // Two pages that share a bucket would confuse the test, so use one
        // page and retire two disjoint sub-ranges of it.
        let page = 0x4500_0000_0000usize;
        let (dead_a, live, dead_b) = (page + 0x10, page + 0x40, page + 0x80);
        assert!(v.insert(dead_a, 0x7400_0000_0001));
        assert!(v.insert(live, 0x7400_0000_0002));
        assert!(v.insert(dead_b, 0x7400_0000_0003));
        let b = v.bucket_of(page);
        assert_eq!(chain_len(&v, b), 3);

        // Retire the two dead ones in one walk, leaving the middle one live.
        v.retire_range(dead_a, dead_a + 1);
        v.retire_range(dead_b, dead_b + 1);

        assert_eq!(
            chain_len(&v, b),
            1,
            "both dead nodes are spliced out and the live one remains"
        );
        assert_eq!(
            v.lookup(live),
            Some(0x7400_0000_0002),
            "the survivor is still findable — the splice must not drop the tail"
        );
        assert_eq!(v.lookup(dead_a), None);
        assert_eq!(v.lookup(dead_b), None);
    }

    /// Recompiling the same page repeatedly does not grow its chain.
    ///
    /// The end-to-end form of the property: `CAP` registrations against one
    /// page, retired between each, and the chain never holds more than the
    /// sites currently live. Before the splice this chain would have been as
    /// long as the number of registrations ever made.
    #[test]
    fn repeated_recompilation_of_one_page_does_not_grow_its_chain() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let page = 0x4600_0000_0000usize;
        let b = v.bucket_of(page);
        // Bounded well below the load factor so no round can decline: the
        // point is the chain length, not exhaustion.
        for round in 0..8usize {
            let site = page + 0x10 + round;
            assert!(v.insert(site, 0x7500_0000_0000 + round), "round {round}");
            assert_eq!(chain_len(&v, b), 1, "round {round}: one live site");
            v.retire_range(site, site + 1);
            assert_eq!(chain_len(&v, b), 0, "round {round}: spliced back to empty");
        }
    }

    /// A probe must not stop at a tombstone.
    ///
    /// A and B collide, so B sits past A in the probe run. Retire A and B must
    /// still be found. If [`Table::lookup`] treated a tombstone as the end of
    /// the run — the single easiest mistake in an open-addressed table — B
    /// would vanish, and in production that is an NPE turning into a process
    /// crash for no visible reason.
    #[test]
    fn a_probe_does_not_stop_at_a_tombstone() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let keys = colliding(0x4300_0000_0000, 64, 2);
        let (a, b) = (keys[0], keys[1]);
        assert!(v.insert(a, 0x7200_0000_0000));
        assert!(v.insert(b, 0x7200_0000_1000));

        v.retire_range(a, a + 1);
        assert_eq!(v.lookup(a), None);
        assert_eq!(
            v.lookup(b),
            Some(0x7200_0000_1000),
            "B hashes to A's slot and lives past it; a probe that stopped at \
             A's tombstone would lose B silently"
        );
    }

    /// The same property through the public entry points and the global table,
    /// so that the wrappers are covered too and not just [`Table`].
    #[test]
    fn the_public_path_skips_a_tombstone_and_still_finds_the_next_site() {
        let keys = colliding(0x6000_0000_0000, CAP, 2);
        let (a, b) = (keys[0], keys[1]);
        assert!(register(a, a + 0x40));
        assert!(register(b, b + 0x40));
        assert_eq!(recover(a, 0), Some(a + 0x40));
        assert_eq!(recover(b, 0), Some(b + 0x40));

        unregister_range(a, 1);
        assert_eq!(recover(a, 0), None, "the retired site is gone");
        assert_eq!(
            recover(b, 0),
            Some(b + 0x40),
            "the site behind the tombstone is not"
        );
        unregister_range(b, 1);
        assert_eq!(recover(b, 0), None);
    }

    /// Past the load factor, registration declines and says so.
    ///
    /// Run on a private table on purpose: filling the process-global one to
    /// prove this would leave it exhausted for every other test in the binary,
    /// which is the same failure mode as the production exhaustion this test
    /// is about.
    #[test]
    fn the_table_declines_past_its_load_factor_and_counts_it() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        assert_eq!(v.load_cap(), 48, "3/4 of 64");

        let mut pc = 0x4400_0000_0000usize;
        for n in 0..48 {
            assert!(v.insert(pc, 0x7300_0000_0000 + n), "site {n} of 48 fits");
            pc += 0x40;
        }
        assert_eq!(t.declined(), 0, "nothing below the cap may decline");

        let declined_before = t.declined();
        assert!(
            !v.insert(pc, 0x7300_0000_9999),
            "the 49th site is past 3/4 load and must be DECLINED, not squeezed \
             in -- an open-addressed table run to capacity has probe runs as \
             long as the scan this design replaced"
        );
        assert_eq!(
            t.declined(),
            declined_before + 1,
            "a decline that is not counted is a feature switching itself off \
             invisibly, which is exactly what `counts` reports DECLINED for"
        );
        assert_eq!(
            v.occupied.load(Ordering::Relaxed),
            48,
            "a declined registration must give its reservation back"
        );
        // And the decline is stable: the table does not spin or eventually
        // relent.
        assert!(!v.insert(pc, 0x7300_0000_9999));
        assert_eq!(t.declined(), declined_before + 2);
    }

    /// Hazard 5: elision stops `headroom_of(cap)` slots BEFORE the load cap,
    /// so the compiles already past their elision decision when the gate
    /// closes still find a slot, instead of declining after their unchecked
    /// load is emitted.
    #[test]
    fn elision_stops_before_the_table_can_decline() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        // Load cap 48, headroom 8: the gate closes at 40 claimed slots.
        assert_eq!(v.load_cap(), 48);
        assert_eq!(headroom_of(64), 8);
        let mut pc = 0x4B00_0000_0000usize;
        for n in 0..40usize {
            assert!(v.has_headroom(), "slot {n}: there is still room to elide");
            assert!(v.insert(pc, 0x7800_0000_0000 + n));
            pc += 0x40;
        }
        assert!(
            !v.has_headroom(),
            "40 of 48 claimed: new compiles must take explicit checks"
        );
        // The in-flight registrations the headroom exists for all fit.
        for n in 40..48usize {
            assert!(v.insert(pc, 0x7800_0000_0000 + n), "in-flight site {n}");
            pc += 0x40;
        }
        assert_eq!(t.declined(), 0);
    }

    /// A live key registered again with a DIFFERENT recovery address is a
    /// stale entry from a buffer that was freed without retiring its sites.
    /// Declining left that entry to answer for the new code's unchecked load
    /// — a jump into the dead buffer's recovery address. It is retired and
    /// the new site registered instead. The identical mapping, by contrast, is
    /// accepted in place without touching the slot.
    #[test]
    fn a_stale_duplicate_is_retired_not_left_to_answer_for_the_new_code() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let pc = 0x4C00_0000_0010usize;
        assert!(v.insert(pc, 0x7900_0000_0001));
        assert!(
            v.insert(pc, 0x7900_0000_0002),
            "the new site must be registered"
        );
        assert_eq!(
            v.lookup(pc),
            Some(0x7900_0000_0002),
            "the fault must resume at THIS method's recovery address, never \
             the freed buffer's"
        );
        assert_eq!(t.retired(), 1);
        assert_eq!(t.declined(), 0);

        let before = v.occupied.load(Ordering::Relaxed);
        assert!(v.insert(pc, 0x7900_0000_0002));
        assert_eq!(
            v.occupied.load(Ordering::Relaxed),
            before,
            "a repeated identical registration claims nothing"
        );
        assert_eq!(v.lookup(pc), Some(0x7900_0000_0002));
    }

    /// Tombstones count against the load factor, because they cost a probe.
    #[test]
    fn retired_entries_still_occupy_their_share_of_the_load_factor() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let base = 0x4500_0000_0000usize;
        for n in 0..48 {
            assert!(v.insert(base + n * 0x40, 0x7400_0000_0000 + n));
        }
        v.retire_range(base, base + 48 * 0x40);
        assert_eq!(t.retired(), 48);
        assert!(
            !v.insert(0x4600_0000_0000, 0x7400_0000_9999),
            "retiring does not give capacity back -- a tombstone is not a free \
             slot, and pretending it is would be the slot reuse hazard 2 rules \
             out"
        );
    }

    /// Retirement walks the chains of the pages a range covers, so a range on
    /// an unrelated page must not disturb it.
    #[test]
    fn retirement_touches_only_the_range_it_was_given() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let keep = 0x4700_0000_0000usize;
        let doomed = 0x4700_0100_0000usize; // 16 MiB away: a different page.
        assert!(v.insert(keep, 0x7500_0000_0000));
        assert!(v.insert(doomed, 0x7500_0000_1000));

        v.retire_range(doomed, doomed + 0x1000);
        assert_eq!(v.lookup(doomed), None);
        assert_eq!(
            v.lookup(keep),
            Some(0x7500_0000_0000),
            "a page-chain walk must not retire entries outside the range"
        );
    }

    /// A range too wide for the chain walk falls back to a sweep, and the
    /// sweep has to retire exactly the same entries.
    #[test]
    fn an_absurdly_wide_range_falls_back_to_a_sweep_with_the_same_result() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let base = 0x4800_0000_0000usize;
        let inside = base + 0x10;
        let outside = base + ((MAX_RANGE_PAGES + 8) << PAGE_SHIFT);
        assert!(v.insert(inside, 0x7600_0000_0000));
        assert!(v.insert(outside, 0x7600_0000_1000));

        let span = (MAX_RANGE_PAGES + 4) << PAGE_SHIFT;
        assert!(
            (span >> PAGE_SHIFT) > MAX_RANGE_PAGES,
            "this test is only meaningful if it takes the sweep path"
        );
        v.retire_range(base, base + span);
        assert_eq!(v.lookup(inside), None);
        assert_eq!(
            v.lookup(outside),
            Some(0x7600_0000_1000),
            "the sweep must respect the range bounds just as the walk does"
        );
    }

    /// A zero-length range is a no-op rather than a sweep of everything.
    #[test]
    fn a_zero_length_range_retires_nothing() {
        let t = OwnedTable::new(64, 16);
        let v = t.view();
        let pc = 0x4900_0000_0000usize;
        assert!(v.insert(pc, 0x7700_0000_0000));
        v.retire_range(pc, pc);
        assert_eq!(v.lookup(pc), Some(0x7700_0000_0000));
        assert_eq!(t.retired(), 0);
    }

    /// The runtime half of the entry-is-base invariant.
    #[test]
    fn note_registration_base_accepts_an_entry_that_is_the_base() {
        assert!(note_registration_base(0x4A00_0000_0000, 0x4A00_0000_0000));
        // The mismatching arm trips a `debug_assert`, which is the point of it,
        // so it is deliberately not exercised here: a test that asserted the
        // `false` return would have to be `#[cfg(not(debug_assertions))]` and
        // would therefore never run where it matters. The compile-time test
        // below is the one that guards the invariant.
        assert_eq!(
            BASE_MISMATCHES.load(Ordering::Relaxed),
            0,
            "nothing in this binary should be registering with a moved entry"
        );
    }

    /// The compile-time half: `x64::driver` must still put the prologue at
    /// offset 0.
    ///
    /// The entire retirement story rests on `entry == buffer_base`. If the
    /// prologue ever acquires a non-zero offset, `CompiledMethod::drop` retires
    /// `[buffer_base, buffer_base + pos)` while `register` filed the sites
    /// under `entry + fault_off`, and every site below the new entry silently
    /// stops being retired — a stale recovery address in a buffer the allocator
    /// has handed to somebody else, which is precisely hazard 2.
    ///
    /// Nothing about that is visible at the call site, and it would not fail a
    /// single functional test; it would fail as an unreproducible jump into the
    /// wrong method, months later. So the invariant is read out of the source
    /// and asserted here.
    ///
    /// The needles are assembled at runtime, in the house style of
    /// `jit/tests/single_bytecode_decoder_ratchet.rs`, so that neither this
    /// file nor a future ratchet scanning `jit/src` can match on the literal.
    #[test]
    fn the_method_entry_is_still_the_buffer_base() {
        const DRIVER: &str = include_str!("x64/driver.rs");
        let decl = format!("let {}_{} ", "entry", "offset");
        let zero = format!("{}= {};", decl, 0);

        let decls: Vec<&str> = DRIVER
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with(decl.as_str()))
            .collect();

        assert_eq!(
            decls.len(),
            1,
            "expected exactly one `{}` declaration in x64/driver.rs, found {}: \
             {decls:?} -- this test reads that declaration to prove the method \
             entry is still the buffer base, and it cannot do that if it cannot \
             find it",
            decl.trim(),
            decls.len()
        );
        assert!(
            decls[0].starts_with(zero.as_str()),
            "x64/driver.rs now says `{}`, not `{}`.\n\n\
             The method entry is no longer the buffer base. \
             `implicit_null::register` keys sites off `entry + fault_off` while \
             `CompiledMethod::drop` retires `[buffer_base, buffer_base + pos)`, \
             so every site below the new entry has just stopped being retired. \
             Those entries outlive their buffer, and the next `alloc_executable` \
             to hand out the same address gives the signal handler a PC match \
             into somebody else's code -- not a crash, but a silent jump to a \
             recovery address inside a live method.\n\n\
             If the prologue really must move, retirement has to move with it: \
             `CompiledMethod::drop` must retire the buffer's whole range \
             independently of `entry`, and the OSR-trampoline purge in that same \
             `Drop` leans on the identity too.",
            decls[0],
            zero
        );
    }

    fn recover_at(pc: usize, addr: usize) -> Option<usize> {
        recover(pc, addr)
    }
}

// REVIEW-NOTE: one thing that belongs in other files.
//
// (Item 1 of this note -- `note_registration_base` had no caller -- is done:
// the driver's registration block calls it since round 9 wave 2, and refuses
// the artifact on a mismatch instead of installing it unregistered.)
//
// 2. `unregister_range` is sub-linear via per-page chains, which is as far as
//    a single file can get. The stronger design is per-*registration* rather
//    than per-page: `CompiledMethod` knows exactly which sites it registered,
//    so `register` could return the slot index it claimed, the driver could
//    keep that small `Vec<u32>` in the `CompiledMethod`, and `drop` could
//    retire exactly those slots -- O(sites in this method) with no chain walk,
//    no dead nodes accumulating on a hot page's bucket, and no `MAX_RANGE_PAGES`
//    fallback. That needs a new `register_at` returning `Option<u32>`, a field
//    on `CompiledMethod` in `jit/src/lib.rs`, and the driver loop to collect it;
//    `unregister_range` would stay for the belt-and-braces sweep. It was not
//    done here because it changes two other files and a public signature.
//    Owner: unassigned -- raise it if a profile ever shows `CompiledMethod::drop`
//    in retirement, which the page chains make unlikely below heavy
//    same-address recompilation.
