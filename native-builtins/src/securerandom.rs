// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.7 — `java.security.SecureRandom` + `java.util.Random` natives.
//!
//! ## Why a dedicated module
//!
//! The previous register-site (`lib.rs::register_security_natives`) aliased
//! `java/util/Random.next*` to the OS-CSPRNG-backed `native_sr_next_*`
//! helpers.  That made `Random` non-deterministic, breaking the documented
//! JDK contract: `new Random(seed).nextLong()` MUST return the same value
//! every time across all JVM implementations (JLS / `j.u.Random`
//! javadoc — "Two instances of Random created with the same seed will
//! produce the same sequence of numbers").
//!
//! This module supplies:
//!
//! * **SecureRandom (SHA1PRNG)** — the one algorithm the JDK specifies as a
//!   deterministic function of its seed. Once `setSeed` is called on an
//!   instance obtained from `getInstance("SHA1PRNG")`, this module runs the
//!   real `sun.security.provider.SecureRandom` state machine so the stream
//!   replays exactly as it does on HotSpot. See the SHA1PRNG section below.
//! * **SecureRandom (everything else)** — backed directly by the OS CSPRNG.  Linux uses
//!   `/dev/urandom` (preferred over `getrandom(2)` because the latter
//!   would block for ≈1s during early-boot fixtures, and
//!   `/dev/urandom` is fed by the same entropy pool past initialization).
//!   Windows uses `BCryptGenRandom` (Cryptography NG, available on every
//!   supported Windows release).  No fallback PRNG is used — if the OS
//!   entropy source fails, the call returns an error rather than silently
//!   downgrading.
//! * **Random** — JDK-spec linear-congruential generator
//!   (`(seed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1)`) that produces a
//!   bit-identical sequence to `java.util.Random` for the same seed.  Seed
//!   state lives in a process-wide side-table keyed on `ObjectRef`
//!   identity, so we never have to assume anything about the real-JDK
//!   `Random` class layout (which uses a private `AtomicLong seed` field
//!   with reflection-hostile naming). Since round 14 wave 3 (SH3-2,
//!   `CRATONVM_RANDOM_REAL_SEED_FIELD`) an exact `Random` on a real JDK keeps
//!   its state in that real field instead; the side table below serves the
//!   synthetic JDK and the fallbacks (see "The real `seed` field IS the state").
//!
//! ## Why a side-table for Random seed
//!
//! In synthetic-jdk mode `Random` is a 2-field synthetic and we could store
//! the seed directly in field 0.  In real-JDK mode, however, `Random.seed`
//! is a private `AtomicLong` and field 0 is its boxed reference — writing
//! a `Long` into that slot would corrupt the object.  The side-table side-
//! steps the question entirely: it keys on a GC-stable per-object key (see
//! "The key" below), works in both modes, and is cheap (one key lookup and a
//! single `RwLock<FxHashMap<usize, u64>>` access per native call).
//!
//! Bug C12 (Round-9): previously this table was keyed on
//! `ObjectRef::as_ptr() as usize`, which silently corrupted any `Random`
//! that survived a GC compaction — the relocated object's pointer no
//! longer matched the stored row, so `lcg_next` fell into the
//! "no stored seed → install OS entropy" lazy-init branch and returned a
//! totally different deterministic stream even though `setSeed()` was
//! never called.  That broke the JDK contract
//! (`new Random(s).nextLong()` must replay identically for the same `s`).
//! Switching to `NativeContext::identity_hash_code(obj)` makes the key
//! GC-stable; the same pattern is used by `VH_META_TABLE` in
//! `lang_invoke.rs`.
//!
//! ## The key (gc-common w29-b)
//!
//! The identity hash was the key until gc-common w29-b
//! (`common-w28b-remaining-identity-hash-keyed-side-tables`, rank 3): three
//! process-wide `FxHashMap<i32, ..>` tables with no VM in the key. Identity
//! hashes are 32 bits, minted per heap from a counter that starts at the same
//! value in every heap and wraps, so two `Random` / seeded `SecureRandom`
//! instances -- of one VM after the wrap, or of two VMs at once -- shared one
//! generator state: correlated output, and one instance's draws advancing the
//! other's stream. The rows were evicted through the VM-less, ZGC-only
//! `cratonvm_types::identity_side_tables` channel, so on Generational and G1
//! they never went, and on ZGC one VM's garbage evicted another VM's live
//! state (`docs/feature-designs/profile-guided-inlining.md`).
//!
//! Every table is now keyed by the instance's WEAK lock key
//! (`crate::gc_stable_weak_lock_key`): minted per VM, unique across VMs and
//! across live identity-hash colliders, stable across moves (the lock-key
//! registry is re-addressed in the stop-the-world epilogue), and freed when
//! the instance dies or its VM goes -- on every backend. The registry then
//! hands the key to [`forget_random_state_keys`], which drops the rows. That
//! replaces the eviction channel, which had no other consumer and was removed.
//!
//! Each native looks its key up ONCE per call, before any table lock (the
//! w20-c `LongAdder` precedent): the lookup takes the process-wide lock-key
//! registry mutex, so a per-draw lookup inside `nextBytes` would take it once
//! per four bytes.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

// Round-9 MED-3: migrated `SEED_TABLE` from `std::sync::RwLock` to
// `parking_lot::RwLock` — removes poison handling (which the file already
// drained with `unwrap_or_else(into_inner)`) and matches the doc comment that
// already claimed parking-lot semantics.
use cratonvm_types::error::MethodCallFailed;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// OS entropy helpers
// ---------------------------------------------------------------------------

/// Fill `buf` with cryptographically-random bytes from the operating
/// system's entropy pool.  Returns `false` only if the OS API itself
/// fails (which is exceedingly rare — e.g. fd exhaustion on Linux, or
/// the kernel's CNG provider being uninstalled on Windows).
///
/// Linux: reads from `/dev/urandom`.  We deliberately do NOT use
/// `getrandom(2)` because on early-boot containers the kernel's
/// blocking pool may not have initialized yet, causing a multi-second
/// stall during VM startup; `/dev/urandom` shares the same entropy pool
/// once initialized but never blocks.
///
/// Windows: uses `BCryptGenRandom` from `bcrypt.dll` with the
/// `BCRYPT_USE_SYSTEM_PREFERRED_RNG` flag, which is the official
/// crypto-grade entropy source on every supported Windows release.
pub fn os_random_bytes(buf: &mut [u8]) -> bool {
    if buf.is_empty() {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        // BCRYPT_USE_SYSTEM_PREFERRED_RNG = 0x00000002.  Per MSDN, this
        // flag means BCryptGenRandom uses the default RNG provider
        // configured by the system, which is the FIPS-validated CNG.
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x00000002;
        #[link(name = "bcrypt")]
        extern "system" {
            fn BCryptGenRandom(
                algorithm: *mut std::ffi::c_void,
                buf: *mut u8,
                buf_len: u32,
                flags: u32,
            ) -> i32;
        }
        // BCryptGenRandom takes a u32 length; for very large buffers
        // (> 4 GiB) we'd need to chunk.  In practice no SecureRandom
        // call requests that much, but we chunk anyway for safety.
        for chunk in buf.chunks_mut(u32::MAX as usize) {
            let status = unsafe {
                BCryptGenRandom(
                    std::ptr::null_mut(),
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                )
            };
            // NTSTATUS 0 == STATUS_SUCCESS.  Any non-zero is a failure
            // — propagate that to the caller so we don't silently
            // downgrade to a weaker source.
            if status != 0 {
                return false;
            }
        }
        true
    }
    #[cfg(not(target_os = "windows"))]
    {
        use std::io::Read;
        // /dev/urandom is the canonical never-block entropy source on
        // Linux, macOS, and the BSDs.  We read in a loop because a
        // single read may return short on a partial-fill (the kernel
        // is allowed to do that under EINTR-style conditions).
        let mut f = match std::fs::File::open("/dev/urandom") {
            Ok(f) => f,
            Err(_) => return false,
        };
        f.read_exact(buf).is_ok()
    }
}

/// Convenience: pull a single u64 of entropy.  Returns `Some` on
/// success, `None` if the OS entropy source failed (caller decides how
/// to surface the error — typically by raising an exception).
pub fn os_random_u64() -> Option<u64> {
    let mut buf = [0u8; 8];
    if os_random_bytes(&mut buf) {
        Some(u64::from_le_bytes(buf))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Random — JDK LCG with side-table seed storage
// ---------------------------------------------------------------------------

/// The constants come straight from the OpenJDK `Random` source —
/// `0x5DEECE66DL` is the multiplier of the linear-congruential generator,
/// `0xBL` is the increment, and the 48-bit mask matches the JDK output
/// space.  These three values together are the "seed contract": any
/// Java program that runs `new Random(s).nextLong()` MUST get the same
/// long across all JVMs, which means we have to use these exact constants.
const LCG_MULTIPLIER: u64 = 0x5DEECE66D;
const LCG_INCREMENT: u64 = 0xB;
const LCG_MASK: u64 = (1u64 << 48) - 1;

/// JDK seed-scrambling rule from `Random.<init>(long)`:
/// `seed = (userSeed ^ 0x5DEECE66DL) & ((1L << 48) - 1)`.
fn scramble_seed(user_seed: i64) -> u64 {
    (user_seed as u64 ^ LCG_MULTIPLIER) & LCG_MASK
}

/// Process-wide map from an instance's weak lock key (see "The key" in the
/// module doc) → 48-bit LCG seed.  Not the raw heap pointer: using it -- what
/// this code did before the Round-9 C12 fix -- silently broke
/// `new Random(seed).nextLong()` after any compaction because the post-move
/// pointer no longer hit the seeded row. Not the identity hash either, since
/// gc-common w29-b: two live instances can share one. Using the side-table at
/// all (rather than embedding state in the object's field 0) keeps the layout
/// independent of class layout, which differs between synthetic-jdk and
/// real-JDK modes.
///
/// The lock is a parking-lot `RwLock` so that concurrent reads (the
/// common case — many threads each holding their own Random) do not
/// serialize on the table; mutations only happen on
/// `<init>` / `setSeed` / inside `lcg_next`, which all need exclusive
/// access for the read-modify-write step.
static SEED_TABLE: RwLock<Option<FxHashMap<usize, u64>>> = RwLock::new(None);

/// Drop the generator state filed under `keys`, which the lock-key registry
/// has just freed: the instance died (any backend, per VM), or its VM is being
/// torn down. The key is never minted again. Called by `lib.rs`'s
/// `sweep_lock_keys` / `forget_vm_lock_keys` (through
/// `forget_core_object_state_keys`) with the registry lock released.
///
/// This is what bounds the three tables now (gc-common w29-b): without it they
/// are append-only for the life of the process -- MEASURED at ~39 bytes
/// retained per `java.util.Random` ever constructed, against a Java heap that
/// stays flat to the kilobyte (`jpalargeblob-random-state-side-table-FIXED-20260830.md`).
/// The collector-driven identity-hash eviction that used to do it ran on ZGC
/// only and carried no VM.
///
/// Each table lock is a leaf, taken and released on its own (SEED ->
/// GAUSSIAN -> SHA1PRNG), and an empty or never-built table is skipped: a
/// program that never constructs a `Random` must not have its maps forced
/// into existence by every collection.
pub fn forget_random_state_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    {
        let mut g = SEED_TABLE.write();
        if let Some(t) = g.as_mut().filter(|t| !t.is_empty()) {
            for k in keys {
                t.remove(k);
            }
        }
    }
    {
        let mut g = GAUSSIAN_TABLE.write();
        if let Some(t) = g.as_mut().filter(|t| !t.is_empty()) {
            for k in keys {
                t.remove(k);
            }
        }
    }
    {
        let mut g = SHA1PRNG_TABLE.write();
        if let Some(t) = g.as_mut().filter(|t| !t.is_empty()) {
            for k in keys {
                t.remove(k);
            }
        }
    }
}

fn with_table_write<R>(f: impl FnOnce(&mut FxHashMap<usize, u64>) -> R) -> R {
    // Round-9 MED-3: parking_lot — no poison handling.
    let mut g = SEED_TABLE.write();
    if g.is_none() {
        *g = Some(FxHashMap::default());
    }
    f(g.as_mut().expect("table just initialized"))
}

/// `java.util.Random.nextNextGaussian` — the second variate of the polar
/// pair, held until the next `nextGaussian()` call consumes it. Keyed exactly
/// like [`SEED_TABLE`] (the weak lock key) because it is part of the
/// same per-instance generator state: an entry's presence is the spec's
/// `haveNextNextGaussian` flag.
static GAUSSIAN_TABLE: RwLock<Option<FxHashMap<usize, f64>>> = RwLock::new(None);

fn with_gaussian_table_write<R>(f: impl FnOnce(&mut FxHashMap<usize, f64>) -> R) -> R {
    let mut g = GAUSSIAN_TABLE.write();
    if g.is_none() {
        *g = Some(FxHashMap::default());
    }
    f(g.as_mut().expect("table just initialized"))
}

/// The key this module files `obj`'s generator state under, minting it on
/// first use: its weak lock key (gc-common w29-b; see "The key" in the module
/// doc). For the paths that WRITE state (`<init>`, `setSeed`, a draw).
///
/// Round-9 C12 made the key move-stable (it was `obj.as_ptr() as usize`); the
/// lock-key registry keeps that property -- it re-addresses its slots in the
/// stop-the-world epilogue -- and adds uniqueness per live object and per VM.
fn random_state_key(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// [`random_state_key`] WITHOUT minting: `None` when `obj` was never keyed, so
/// it has no row in any table here. For the read-only SHA1PRNG draw paths,
/// where nearly every receiver is an unseeded (OS-CSPRNG) instance that must
/// not leave a registry slot behind per call.
fn existing_random_state_key(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
    crate::existing_weak_lock_key(ctx, obj)
}

/// Install the user's seed for this object.  Used by the constructors;
/// `setSeed` goes through [`set_seed_for_key`].
fn set_seed(ctx: &mut dyn NativeContext, obj: ObjectRef, user_seed: i64) {
    set_seed_for_key(random_state_key(ctx, obj), user_seed);
}

/// [`set_seed`] on an already-looked-up key.
fn set_seed_for_key(key: usize, user_seed: i64) {
    let scrambled = scramble_seed(user_seed);
    with_table_write(|t| {
        t.insert(key, scrambled);
    });
    // `java.util.Random.setSeed` also clears `haveNextNextGaussian`, so a
    // re-seeded generator must not hand back a variate drawn from the old seed.
    with_gaussian_table_write(|t| {
        t.remove(&key);
    });
}

/// `java.util.Random`'s seed uniquifier, verbatim from the JDK:
///
/// ```text
/// private static long seedUniquifier() {
///     for (;;) {
///         long current = seedUniquifier.get();
///         long next = current * 1181783497276652981L;
///         if (seedUniquifier.compareAndSet(current, next)) return next;
///     }
/// }
/// private static final AtomicLong seedUniquifier = new AtomicLong(8682522807148012L);
/// ```
///
/// Both constants are the JDK's. The multiply is what makes successive
/// no-arg constructions diverge even when `System.nanoTime()` has not ticked
/// between them, which is the property the old OS-entropy draw was really
/// buying and the reason a bare timestamp would not do.
static SEED_UNIQUIFIER: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(8682522807148012);

fn seed_uniquifier() -> i64 {
    use std::sync::atomic::Ordering;
    loop {
        let current = SEED_UNIQUIFIER.load(Ordering::Relaxed);
        let next = current.wrapping_mul(1181783497276652981);
        if SEED_UNIQUIFIER
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

/// Install the seed for an unseeded `new Random()`.
///
/// `java.util.Random()` is SPECIFIED as `this(seedUniquifier() ^
/// System.nanoTime())`, and that is now what this does. It used to draw from
/// the OS CSPRNG — `BCryptGenRandom` on Windows, an `open`+`read` of
/// `/dev/urandom` on Linux — once per construction.
///
/// Two reasons that was wrong, in order of importance:
///
/// 1. **It is not what the spec says.** `java.util.Random` is documented as not
///    cryptographically secure; drawing from a CSPRNG for it buys no property
///    any caller may rely on, and the JDK's own algorithm is public and cheap.
/// 2. **It was a syscall per construction**, on a class whose whole point is to
///    be cheap. `JpaLargeBlobTest.jpaBlobStream`'s fixture calls `new Random()`
///    once PER BYTE of a 100,000,000-byte stream, so the draw was a syscall per
///    byte. MEASURED on this host, `probes/BlobStreamCost.java`: the unseeded
///    constructor cost 982.2 ns/op against the seeded one's 832.0 — the ~150 ns
///    gap is this draw.
///
/// Distinctness is preserved and is still checked: `probes/RandomSpec.java`'s
/// `unseeded-distinct` row asserts two successive `new Random()` instances
/// produce different sequences, which the uniquifier's multiply guarantees
/// without consulting the clock at all.
fn set_entropy_seed(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    set_seed(ctx, obj, seed_uniquifier() ^ nanos);
}

/// Run one LCG step on this object's stored seed and return the top
/// `bits` bits.  This is the JDK's protected `next(int bits)` method:
///
/// ```text
/// seed = (seed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
/// return (int)(seed >>> (48 - bits));
/// ```
///
/// `bits` is always between 1 and 32; the caller is responsible for
/// the contract.  If the object has no stored seed yet (e.g. the
/// constructor was missed), we lazily install an entropy seed so we
/// never return zero/predictable output.
///
/// Takes the instance's [`random_state_key`], which the native looks up once
/// per call (gc-common w29-b): `nextBytes` makes one step per four bytes and
/// the rejection loops of `nextInt(bound)` / `nextGaussian` several, and a
/// key lookup takes the process-wide lock-key registry mutex.
fn lcg_next(key: usize, bits: u32) -> i32 {
    let new_seed = with_table_write(|t| {
        let old = match t.get(&key) {
            Some(s) => *s,
            None => {
                // Lazy initialization — defensive: the constructor always
                // seeds, but if it was somehow missed we must not emit zeros
                // forever. Uses the same rule as `set_entropy_seed` so the
                // module has ONE seeding policy rather than two that disagree
                // about whether `java.util.Random` needs a CSPRNG (it does
                // not, and this path used to spend a syscall deciding so).
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(1);
                let s = scramble_seed(seed_uniquifier() ^ nanos);
                t.insert(key, s);
                s
            }
        };
        let next = old.wrapping_mul(LCG_MULTIPLIER).wrapping_add(LCG_INCREMENT) & LCG_MASK;
        t.insert(key, next);
        next
    });
    (new_seed >> (48 - bits)) as i32
}

// ---------------------------------------------------------------------------
// Subclass receivers run `java.util.Random`'s own bytecode
// ---------------------------------------------------------------------------
//
// Round 13 wave 9 (lane shadow3),
// `r13w8-shadow2-random-draw-natives-ignore-an-overridden-next-FIXED-20260928.md`.
// `Random` specifies every draw in terms of the protected `next(int)`, and
// several draws in terms of each other (`nextBytes` calls `nextInt()`,
// `nextGaussian` calls `nextDouble()`), all virtually. The natives below
// compute the draws straight from the side table, which is only the JDK's
// answer when no override can be in the way, i.e. for an EXACT
// `java.util.Random` receiver. For a proper subclass they stand down to
// `Random`'s own body (invokespecial semantics, so a subclass's
// `super.nextInt()` does not bounce back into its override), and the
// constructors and `setSeed` stand down too, so the real `seed` AtomicLong is
// built and a `super.next(bits)` finds a real generator. A subclass receiver
// therefore never gets a side-table row, and an exact one always has one after
// its native `<init>`: a draw that finds its row skips the class check.

const RANDOM_CLASS: &str = "java/util/Random";



/// True when `this` is an instance of a PROPER subclass of a
/// `java.util.Random` that has real bytecode (`next(I)I` with a body). False
/// for an exact `Random`, for the synthetic JDK's code-less `Random`, and for
/// a context that cannot name the class (the unit mocks).
fn random_receiver_runs_bytecode(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let Some(random) = ctx.class_id_by_name(RANDOM_CLASS) else {
        return false;
    };
    let class = ctx.class_id_of_object(this);
    class != random
        && ctx.is_subclass(class, random)
        && ctx.class_declares_method(random, "next", "(I)I")
}

fn seed_row_exists(key: usize) -> bool {
    SEED_TABLE
        .read()
        .as_ref()
        .is_some_and(|t| t.contains_key(&key))
}

/// The side-table key a `setSeed` or draw native serves `this` from, or `None`
/// when `this` must run `Random`'s own bytecode instead (see the section note
/// above). One lock-key lookup on the fast path, as before.
fn random_native_key(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    if let Some(key) = existing_random_state_key(ctx, this) {
        if seed_row_exists(key) {
            return Some(key);
        }
    }
    if random_receiver_runs_bytecode(ctx, this) {
        return None;
    }
    let key = random_state_key(ctx, this);
    {
        adopt_real_random_state(ctx, this, key);
    }
    Some(key)
}

// ---------------------------------------------------------------------------
// The real `seed` field IS the state (round 14 wave 3, SH3-2)
// ---------------------------------------------------------------------------
//
// Proposal SH3-2 of `jit-r13-shadow3-proposals-RETIRED-20260929.md`. On a real JDK an EXACT
// `java.util.Random` is now built by its own constructor bytecode (which
// allocates the `AtomicLong seed` exactly as HotSpot does), and every native
// draw steps that `AtomicLong`'s `value` slot with a CAS, as `Random.next`
// does. `setSeed` / `nextGaussian` read and write the real
// `haveNextNextGaussian` / `nextNextGaussian` fields; both are `synchronized`
// in the JDK and a native answering a synchronized method runs holding its
// monitor (`native_sync_enabled`), so they need no table lock.
//
// So real bytecode and the natives can never disagree about the state any
// more (serialization, reflection, `Random.next` reached from bytecode), and
// such an instance mints no lock key and no side-table row at all. The side
// table stays for the synthetic JDK (no bytecode to build the field), for a
// subclass under `CRATONVM_RANDOM_SUBCLASS_YIELD=0`, and as the fallback when
// a receiver has no `AtomicLong` (a constructor that did not build one).



/// An exact `Random`'s real generator cell: the `AtomicLong` its `seed` holds
/// and the slot of that `AtomicLong`'s `value`.
#[derive(Clone, Copy)]
struct SeedCell {
    random: cratonvm_types::ClassId,
    atomic: ObjectRef,
    value: usize,
}

/// `this`'s [`SeedCell`] when it is an EXACT `java.util.Random` whose `seed`
/// holds an `AtomicLong` with a `long` `value` (the real JDK layout), else
/// `None` (a subclass, the synthetic `Random`, a receiver whose constructor
/// built no `AtomicLong`, or the switch off). Reads only.
fn exact_random_seed_cell(ctx: &dyn NativeContext, this: ObjectRef) -> Option<SeedCell> {
    let vm = ctx.vm_identity();
    if let Some(cell) = memoized_random_seed_cell(ctx, vm, this) {
        return Some(cell);
    }
    let random = ctx.class_id_by_name(RANDOM_CLASS)?;
    if ctx.class_id_of_object(this) != random {
        return None;
    }
    let seed_slot = ctx.resolve_field_index_by_class_id(random, "seed")?;
    if seed_slot >= ctx.object_num_fields(this) {
        return None;
    }
    let Value::Object(Some(atomic)) = ctx.get_field(this, seed_slot) else {
        return None;
    };
    let atomic_class = ctx.class_id_of_object(atomic);
    let value = ctx.resolve_field_index_by_class_id(atomic_class, "value")?;
    if value >= ctx.object_num_fields(atomic) {
        return None;
    }
    if !matches!(ctx.get_field(atomic, value), Value::Long(_)) {
        return None;
    }
    if vm != 0 {
        let layout = RandomLayout {
            vm,
            random,
            seed_slot,
            atomic_class,
            value_slot: value,
        };
        RANDOM_LAYOUT.with(|memo| memo.set(Some(layout)));
    }
    Some(SeedCell {
        random,
        atomic,
        value,
    })
}

// Round 14 wave 5 (lane fixup2, proposal RV4-2 of
// `jit-r14-review4-proposals-RETIRED-20260930.md`): the layout [`exact_random_seed_cell`]
// derives -- `Random`'s class id, its `seed` slot, the `AtomicLong` class id
// and its `value` slot -- memoised, so a draw on an exact `Random` costs three
// class/slot compares instead of a class lookup by name plus two field-index
// resolutions per native call. Per thread and keyed by `vm_identity`: VM
// identities are never recycled and a real VM's is never 0 (a unit mock's is,
// and mocks never memoise), so a row can only be served to the VM that filled
// it, and no lock is shared between drawing threads. A class id and a field
// slot are fixed for a VM's life (redefinition cannot change a class's
// fields), and every object read is still checked: a miss on any compare
// falls back to the full derivation, so the answer is the uncached one.



/// One VM's exact-`Random` layout, as [`exact_random_seed_cell`] derived it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RandomLayout {
    vm: usize,
    random: cratonvm_types::ClassId,
    seed_slot: usize,
    atomic_class: cratonvm_types::ClassId,
    value_slot: usize,
}

thread_local! {
    static RANDOM_LAYOUT: std::cell::Cell<Option<RandomLayout>> =
        const { std::cell::Cell::new(None) };
}

/// [`exact_random_seed_cell`]'s answer from this thread's [`RandomLayout`] for
/// VM `vm`, or `None` to derive it (no row for `vm`, the switch off, or any
/// check failing: a subclass receiver, a `seed` that is not the memoised
/// `AtomicLong`, a `value` that is not a `long`).
fn memoized_random_seed_cell(
    ctx: &dyn NativeContext,
    vm: usize,
    this: ObjectRef,
) -> Option<SeedCell> {
    if vm == 0 {
        return None;
    }
    let layout = RANDOM_LAYOUT
        .with(|memo| memo.get())
        .filter(|l| l.vm == vm)?;
    if ctx.class_id_of_object(this) != layout.random
        || layout.seed_slot >= ctx.object_num_fields(this)
    {
        return None;
    }
    let Value::Object(Some(atomic)) = ctx.get_field(this, layout.seed_slot) else {
        return None;
    };
    if ctx.class_id_of_object(atomic) != layout.atomic_class
        || layout.value_slot >= ctx.object_num_fields(atomic)
    {
        return None;
    }
    matches!(ctx.get_field(atomic, layout.value_slot), Value::Long(_)).then_some(SeedCell {
        random: layout.random,
        atomic,
        value: layout.value_slot,
    })
}

/// Whether a `Random` constructor native runs the real constructor for an
/// EXACT receiver so that it builds the real `AtomicLong seed`: the switch is
/// on and this `Random` has real bytecode with the real field (never the
/// synthetic JDK's code-less `Random`, nor the unit mocks').
fn random_ctor_builds_real_seed(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let Some(random) = ctx.class_id_by_name(RANDOM_CLASS) else {
        return false;
    };
    ctx.class_id_of_object(this) == random
        && ctx.class_declares_method(random, "next", "(I)I")
        && ctx
            .resolve_field_index_by_class_id(random, "seed")
            .is_some()
}

/// Run the real `Random.<init>descriptor` for an exact receiver (see
/// [`random_ctor_builds_real_seed`]). `Ok(Some(this))` (its current address)
/// when it left no `AtomicLong` behind, so the caller seeds the side table
/// instead and a draw never finds a receiver with no state at all.
fn random_ctor_real_seed(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    descriptor: &str,
    args: &[Value],
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // The constructor allocates (`new AtomicLong`): carry `this` across it.
    let pin = ctx.pin_native_root(this);
    let result = random_run_bytecode(ctx, "<init>", descriptor, args);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    result?;
    Ok(exact_random_seed_cell(&*ctx, this)
        .is_none()
        .then_some(this))
}

/// One generator step on the real cell: `Random.next(bits)`'s CAS loop.
fn field_lcg_next(ctx: &mut dyn NativeContext, cell: SeedCell, bits: u32) -> i32 {
    loop {
        let current = ctx.get_field_volatile(cell.atomic, cell.value);
        let old = match current {
            Value::Long(v) => v as u64,
            _ => 0,
        };
        let next = old.wrapping_mul(LCG_MULTIPLIER).wrapping_add(LCG_INCREMENT) & LCG_MASK;
        let stored = Value::Long(next as i64);
        let won = if matches!(current, Value::Long(_)) {
            ctx.compare_and_swap_field(cell.atomic, cell.value, current, stored)
        } else {
            // Not a `long` after all ([`exact_random_seed_cell`] checked it
            // was): store rather than spin on a CAS that cannot succeed.
            ctx.set_field_volatile(cell.atomic, cell.value, stored);
            true
        };
        if won {
            return (next >> (48 - bits)) as i32;
        }
        std::hint::spin_loop();
    }
}

/// Where a draw native takes its generator steps from.
#[derive(Clone, Copy)]
enum Lcg {
    /// The side-table row under this key.
    Table(usize),
    /// The receiver's real `seed` field.
    Field(SeedCell),
}

/// The generator a draw / `setSeed` native serves `this` from, or `None` when
/// `this` must run `Random`'s own bytecode (a subclass; see
/// [`random_native_key`]). The real field first: an exact receiver that has
/// one never has a side-table row it would disagree with (its constructor
/// built no row, and [`native_random_write_object`] hands a row over to the
/// fields it materializes), and it pays no lock-key lookup.
fn random_lcg(ctx: &dyn NativeContext, this: ObjectRef) -> Option<Lcg> {
    if let Some(cell) = exact_random_seed_cell(ctx, this) {
        return Some(Lcg::Field(cell));
    }
    random_native_key(ctx, this).map(Lcg::Table)
}

fn lcg_step(ctx: &mut dyn NativeContext, lcg: Lcg, bits: u32) -> i32 {
    match lcg {
        Lcg::Table(key) => lcg_next(key, bits),
        Lcg::Field(cell) => field_lcg_next(ctx, cell, bits),
    }
}

/// The slots of `haveNextNextGaussian` and `nextNextGaussian` on the real
/// `Random`, when both resolve.
fn gaussian_slots(ctx: &dyn NativeContext, cell: SeedCell) -> Option<(usize, usize)> {
    Some((
        ctx.resolve_field_index_by_class_id(cell.random, "haveNextNextGaussian")?,
        ctx.resolve_field_index_by_class_id(cell.random, "nextNextGaussian")?,
    ))
}

// ---------------------------------------------------------------------------
// The real fields, for serialization
// ---------------------------------------------------------------------------
//
// Round 13 wave 10 (lane misc10),
// `r13w9-shadow3-random-serialization-ignores-the-side-table-FIXED-20260928.md`.
// `Random.readObject` / `writeObject` are real bytecode over the real
// `AtomicLong seed`, `nextNextGaussian` and `haveNextNextGaussian`, which the
// natives above never touch. Two bridges keep the side table and the fields in
// step at exactly the two points a stream crosses:
//
// * a receiver with NO row (it never went through a native `<init>`: a
//   deserialized one, or one built by `Unsafe.allocateInstance`) ADOPTS its
//   real fields on its first native call ([`adopt_real_random_state`]);
// * `writeObject` first MATERIALIZES the row into the real fields
//   ([`native_random_write_object`]), then runs the real body.
//
// Outside those two points the side table stays authoritative.



/// Install `this`'s real generator state as its side-table row, when it has a
/// real one: a non-null `seed` whose `value` is a long (the real JDK layout;
/// the synthetic `Random` and the unit mocks have no such field, and then this
/// does nothing and `lcg_next` seeds lazily as before). An existing row wins,
/// so a racing draw that installed one first is never rewound. Reads only; no
/// allocation.
fn adopt_real_random_state(ctx: &dyn NativeContext, this: ObjectRef, key: usize) {
    let Value::Object(Some(atomic)) = ctx.get_field_by_name(this, "seed") else {
        return;
    };
    let Value::Long(raw) = ctx.get_field_by_name(atomic, "value") else {
        return;
    };
    // `next` masks after the multiply, so only the low 48 bits carry state.
    let state = raw as u64 & LCG_MASK;
    let inserted = with_table_write(|t| match t.entry(key) {
        std::collections::hash_map::Entry::Occupied(_) => false,
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(state);
            true
        }
    });
    if !inserted {
        return;
    }
    let have =
        matches!(ctx.get_field_by_name(this, "haveNextNextGaussian"), Value::Int(v) if v != 0);
    if have {
        if let Value::Double(g) = ctx.get_field_by_name(this, "nextNextGaussian") {
            with_gaussian_table_write(|t| {
                t.insert(key, g);
            });
        }
    }
}

const RANDOM_WRITE_OBJECT_DESC: &str = "(Ljava/io/ObjectOutputStream;)V";

/// `private synchronized void writeObject(ObjectOutputStream)` on an exact
/// `java.util.Random` (registered on a real JDK only; see the registrar).
///
/// The real body serializes `seed.get()` and the two Gaussian fields, which a
/// natively constructed instance never wrote: `seed` is null (NPE) and the
/// Gaussian cache lives in [`GAUSSIAN_TABLE`]. When `this` has a side-table
/// row, write it into the real fields first -- through the existing
/// `AtomicLong` when there is one, else through `Random.resetSeed(long)`, the
/// JDK's own allocation of it (the same one `readObject` uses) -- then run the
/// real body. A receiver with no row (a subclass, or a deserialized instance
/// that never drew) already has consistent real fields.
pub fn native_random_write_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (this, stream) = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(o))), Some(s)) => (*o, *s),
        _ => return random_run_bytecode(ctx, "writeObject", RANDOM_WRITE_OBJECT_DESC, args),
    };
    let row = existing_random_state_key(ctx, this).and_then(|key| {
        SEED_TABLE
            .read()
            .as_ref()
            .and_then(|t| t.get(&key).copied())
            .map(|state| (key, state))
    });
    // SH3-2: an EXACT receiver's materialized fields become its state (the
    // draws read the real field first, `random_lcg`), so its row is handed
    // over -- dropped -- once they hold it; a second `writeObject` must not
    // rewind the fields to the row. A subclass keeps its row authoritative.
    let hands_over = ctx
            .class_id_by_name(RANDOM_CLASS)
            .is_some_and(|random| random == ctx.class_id_of_object(this));
    let (state, handover_key) = match row {
        Some((key, state)) => {
            let gaussian = GAUSSIAN_TABLE
                .read()
                .as_ref()
                .and_then(|t| t.get(&key).copied());
            ctx.set_field_by_name(
                this,
                "haveNextNextGaussian",
                Value::Int(gaussian.is_some() as i32),
            );
            if let Some(g) = gaussian {
                ctx.set_field_by_name(this, "nextNextGaussian", Value::Double(g));
            }
            if let Value::Object(Some(atomic)) = ctx.get_field_by_name(this, "seed") {
                ctx.set_field_by_name(atomic, "value", Value::Long(state as i64));
                if hands_over {
                    drop_random_rows(key);
                }
                return random_run_bytecode(
                    ctx,
                    "writeObject",
                    RANDOM_WRITE_OBJECT_DESC,
                    &[Value::Object(Some(this)), stream],
                );
            }
            (state, hands_over.then_some(key))
        }
        // No row: the real fields ARE the state -- unless `seed` was never
        // built, which is the `Random` slice of a `SecureRandom` whose native
        // `<init>` skipped `Random`'s. HotSpot's `Random(0)` for a subclass
        // leaves `new AtomicLong()` (0) there (`SecureRandom.setSeed(long)`
        // does not call `super`), so serialize 0 rather than NPE.
        None => match ctx.get_field_by_name(this, "seed") {
            Value::Object(None) => (0, None),
            _ => return random_run_bytecode(ctx, "writeObject", RANDOM_WRITE_OBJECT_DESC, args),
        },
    };
    // `resetSeed` allocates (Java `new AtomicLong`): root the receiver and the
    // stream across it and hand the body their current addresses.
    let this_pin = ctx.pin_native_root(this);
    let stream_pin = match stream {
        Value::Object(Some(s)) => Some(ctx.pin_native_root(s)),
        _ => None,
    };
    let reset = random_run_bytecode(
        ctx,
        "resetSeed",
        "(J)V",
        &[Value::Object(Some(this)), Value::Long(state as i64)],
    );
    let this = ctx.read_native_pin(this_pin, this);
    let stream = match (stream_pin, stream) {
        (Some(pin), Value::Object(Some(s))) => Value::Object(Some(ctx.read_native_pin(pin, s))),
        _ => stream,
    };
    let result = match reset {
        Ok(_) => {
            if let Some(key) = handover_key {
                drop_random_rows(key);
            }
            random_run_bytecode(
                ctx,
                "writeObject",
                RANDOM_WRITE_OBJECT_DESC,
                &[Value::Object(Some(this)), stream],
            )
        }
        Err(e) => Err(e),
    };
    ctx.unpin_native_roots(this_pin);
    result
}

/// Drop `key`'s seed row and cached Gaussian (SH3-2's hand-over to the real
/// fields; see [`native_random_write_object`]).
fn drop_random_rows(key: usize) {
    with_table_write(|t| {
        t.remove(&key);
    });
    with_gaussian_table_write(|t| {
        t.remove(&key);
    });
}

/// Whether a `Random` constructor native must run the real constructor for
/// `this` (a proper subclass): the JDK constructor builds the `seed`
/// AtomicLong and, for a subclass, seeds it through a virtual `setSeed`.
fn random_ctor_runs_bytecode(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    random_receiver_runs_bytecode(ctx, this)
}

/// Run `java.util.Random.<method><descriptor>`'s own body on `args`
/// (receiver first), skipping this native.
fn random_run_bytecode(
    ctx: &mut dyn NativeContext,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    ctx.invoke_special_bytecode_only(RANDOM_CLASS, method, descriptor, args)
}

// ---------------------------------------------------------------------------
// java.util.Random natives
// ---------------------------------------------------------------------------

pub fn native_random_init_noseed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if random_ctor_runs_bytecode(ctx, *this) {
            return random_run_bytecode(ctx, "<init>", "()V", args);
        }
        // SH3-2: `this(seedUniquifier() ^ System.nanoTime())`, the JDK's own.
        if random_ctor_builds_real_seed(&*ctx, *this) {
            if let Some(this) = random_ctor_real_seed(ctx, *this, "()V", args)? {
                set_entropy_seed(ctx, this);
            }
            return Ok(None);
        }
        set_entropy_seed(ctx, *this);
    }
    Ok(None)
}

pub fn native_random_init_seed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if random_ctor_runs_bytecode(ctx, this) {
        return random_run_bytecode(ctx, "<init>", "(J)V", args);
    }
    let seed = match args.get(1) {
        Some(Value::Long(s)) => *s,
        _ => 0,
    };
    // SH3-2: `this.seed = new AtomicLong(initialScramble(seed))`, the JDK's own.
    if random_ctor_builds_real_seed(&*ctx, this) {
        if let Some(this) = random_ctor_real_seed(ctx, this, "(J)V", args)? {
            set_seed(ctx, this, seed);
        }
        return Ok(None);
    }
    set_seed(ctx, this, seed);
    Ok(None)
}

pub fn native_random_set_seed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "setSeed", "(J)V", args);
    };
    let seed = match args.get(1) {
        Some(Value::Long(s)) => *s,
        _ => 0,
    };
    match lcg {
        Lcg::Table(key) => set_seed_for_key(key, seed),
        // `this.seed.set(initialScramble(seed)); haveNextNextGaussian = false;`
        Lcg::Field(cell) => {
            ctx.set_field_volatile(
                cell.atomic,
                cell.value,
                Value::Long(scramble_seed(seed) as i64),
            );
            if let Some((have, _)) = gaussian_slots(&*ctx, cell) {
                ctx.set_field(this, have, Value::Int(0));
            }
        }
    }
    Ok(None)
}

pub fn native_random_next_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "nextInt", "()I", args);
    };
    Ok(Some(Value::Int(lcg_step(ctx, lcg, 32))))
}

pub fn native_random_next_int_bound(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "nextInt", "(I)I", args);
    };
    let bound = match args.get(1) {
        Some(Value::Int(b)) => *b,
        _ => 1,
    };
    if bound <= 0 {
        // CRATONVM_DBG_NEXTINT=1 — dump the Java caller chain when a
        // `Random.nextInt(bound<=0)` is about to throw. The thrown IAE is
        // created in Rust (not via an `athrow` bytecode), so CRATONVM_DBG_ATHROW
        // only catches the downstream rethrow, not this origin. Used to locate
        // the empty-collection / zero-count divergence in Elasticsearch /
        // Lucene test-framework `@BeforeClass` setup (RandomPicks.randomFrom).
        if crate::nbflags().dbg_nextint {
            eprintln!("NEXTINT-BAD bound={bound} caller-chain (inner→outer):");
            let frames = ctx.capture_stack_trace();
            for f in frames.iter().rev().take(20) {
                eprintln!(
                    "  NEXTINT-STK {}.{} line={} bci={}",
                    f.class_name, f.method_name, f.line_number, f.byte_code_index
                );
            }
        }
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "bound must be positive".to_string(),
            }
            .into(),
        );
    }
    let m = bound - 1;
    let mut r = lcg_step(ctx, lcg, 31);
    if (bound & m) == 0 {
        // Power-of-two fast path — JDK formula.
        r = ((bound as i64).wrapping_mul(r as i64) >> 31) as i32;
    } else {
        // Rejection sampling for unbiased distribution.  JDK uses the
        // same loop; bound > 0 so the loop terminates with high
        // probability after one or two iterations.
        loop {
            let candidate = r % bound;
            if r.wrapping_sub(candidate).wrapping_add(m) >= 0 {
                r = candidate;
                break;
            }
            r = lcg_step(ctx, lcg, 31);
        }
    }
    Ok(Some(Value::Int(r)))
}

pub fn native_random_next_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "nextLong", "()J", args);
    };
    // JDK's nextLong is `((long)next(32) << 32) + next(32)`.  We
    // sign-extend each i32 to i64 first to keep the high half wider
    // than 32 bits, exactly matching the JDK output bit-for-bit.
    let hi = lcg_step(ctx, lcg, 32) as i64;
    let lo = lcg_step(ctx, lcg, 32) as i64;
    Ok(Some(Value::Long((hi << 32).wrapping_add(lo))))
}

pub fn native_random_next_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "nextDouble", "()D", args);
    };
    // JDK formula: `(((long)next(26) << 27) + next(27)) / (double)(1L << 53)`.
    let hi = (lcg_step(ctx, lcg, 26) as i64) << 27;
    let lo = lcg_step(ctx, lcg, 27) as i64;
    let v = (hi + lo) as f64 / ((1i64 << 53) as f64);
    Ok(Some(Value::Double(v)))
}

pub fn native_random_next_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "nextFloat", "()F", args);
    };
    // JDK: `next(24) / (float)(1 << 24)`.
    let v = lcg_step(ctx, lcg, 24) as f32 / ((1i32 << 24) as f32);
    Ok(Some(Value::Float(v)))
}

pub fn native_random_next_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "nextBoolean", "()Z", args);
    };
    Ok(Some(Value::Int(lcg_step(ctx, lcg, 1))))
}

pub fn native_random_next_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "nextBytes", "([B)V", args);
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `Random.nextBytes` is specified `@throws NullPointerException if the
        // byte array is null` (`Random.java:458`) and its body opens
        // `bytes.length`. A `Value::Object(None)` here IS that null, and this
        // arm used to swallow it: `new Random(42).nextBytes(null)` returned
        // normally, which `RJdkIntrinsics2 --only=random` check 41 reports as
        // "got none". Message measured on OpenJDK 25.0.3+9.
        Some(Value::Object(None)) => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Cannot read the array length because \"bytes\" is null".to_string()),
            }
            .into());
        }
        // A missing or non-reference argument is an arity/marshalling bug, not
        // a Java null — keep the defensive return rather than reporting an NPE
        // the program did not cause.
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr);
    // JDK fills 4 bytes per LCG draw; we replicate that exactly so the
    // produced byte sequence matches `new Random(seed).nextBytes(buf)`.
    let mut i = 0usize;
    while i < len {
        let mut rnd = lcg_step(ctx, lcg, 32) as i32;
        let n = std::cmp::min(len - i, 4);
        for _ in 0..n {
            // Sign-extend low 8 bits to i32 so the array stores the
            // canonical Java byte (signed 8-bit).
            let b = (rnd & 0xFF) as i8 as i32;
            ctx.set_array_element(arr, i, Value::Int(b));
            rnd >>= 8;
            i += 1;
        }
    }
    Ok(None)
}

pub fn native_random_next_gaussian(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let Some(lcg) = random_lcg(&*ctx, this) else {
        return random_run_bytecode(ctx, "nextGaussian", "()D", args);
    };
    let key = match lcg {
        Lcg::Table(key) => key,
        Lcg::Field(cell) => return field_next_gaussian(ctx, this, cell, args),
    };
    // Marsaglia polar method, as `java.util.Random.nextGaussian()` specifies
    // it — INCLUDING the cached partner.
    //
    // This used to compute the pair and discard the second value, with a
    // comment calling the cache a micro-optimisation "not worth the trouble".
    // That reasoning was wrong: the cache is not an optimisation, it is part of
    // the specified output. Discarding the partner both advanced the LCG at
    // twice the specified rate — so every SEEDED sequence diverged from a
    // conforming VM — and made successive results i.i.d. rather than sharing a
    // `mult`, which is a property real code depends on. H2's `MemoryEstimator`
    // sizes a skip counter from how close consecutive values are, so
    // `TestMemoryEstimator` saw a sampling percentage of 8 against its `<= 7`
    // bound; that failure was recorded in `vm/src/jit/skip_list.rs` as the sole
    // blocker on lifting the `org/h2/` JIT ban and was believed to be a JIT
    // miscompile. It is neither JIT-related nor H2-specific.
    if let Some(cached) = with_gaussian_table_write(|t| t.remove(&key)) {
        return Ok(Some(Value::Double(cached)));
    }
    match rnd_gaussian_pair(&mut |bits| lcg_next(key, bits)) {
        Some((v1, v2)) => {
            with_gaussian_table_write(|t| {
                t.insert(key, v2);
            });
            Ok(Some(Value::Double(v1)))
        }
        None => Ok(Some(Value::Double(0.0))),
    }
}

/// [`native_random_next_gaussian`] on the real fields (SH3-2): the JDK body
/// over `haveNextNextGaussian` / `nextNextGaussian`, drawing from the real
/// `seed`. `nextGaussian` is `synchronized`, and the VM holds its monitor
/// around this native, so the test-and-clear needs no lock of its own.
fn field_next_gaussian(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    cell: SeedCell,
    args: &[Value],
) -> MethodCallResult {
    let Some((have, cached)) = gaussian_slots(&*ctx, cell) else {
        return random_run_bytecode(ctx, "nextGaussian", "()D", args);
    };
    if matches!(ctx.get_field(this, have), Value::Int(v) if v != 0) {
        ctx.set_field(this, have, Value::Int(0));
        return Ok(Some(match ctx.get_field(this, cached) {
            Value::Double(d) => Value::Double(d),
            _ => Value::Double(0.0),
        }));
    }
    let pair = rnd_gaussian_pair(&mut |bits| field_lcg_next(&mut *ctx, cell, bits));
    match pair {
        Some((v1, v2)) => {
            ctx.set_field(this, cached, Value::Double(v2));
            ctx.set_field(this, have, Value::Int(1));
            Ok(Some(Value::Double(v1)))
        }
        None => Ok(Some(Value::Double(0.0))),
    }
}

/// The polar (Marsaglia) method exactly as `java.util.Random.nextGaussian()`
/// specifies it, returning BOTH variates of the accepted pair.
///
/// Factored out of the native for one reason: **the native cannot be tested.**
/// Its draws came through `lcg_next(ctx, …)`, which needed a live
/// `NativeContext` (today it takes the instance's key, minted through one), so
/// every in-crate test of the seeded gaussian stream had to
/// call something else — and "something else" was a second, independent copy of
/// this arithmetic living in `native-collections`. That copy had the `log` fix
/// and a bit-exact test; this one, which is the copy that actually runs, had
/// neither, and the green test on the other copy is what made that invisible for
/// as long as it was. `next` is `Random.next(bits)`, so the sequence is now
/// testable without a VM. See the seeded-sequence test below.
///
/// The multiplier is `StrictMath.sqrt(-2 * StrictMath.log(s) / s)` in the JDK,
/// and the `StrictMath` there is load-bearing: `sqrt`, `*` and `/` are
/// exactly-rounded IEEE 754 and agree everywhere, but `log` is a bit-for-bit
/// fdlibm contract that platform libm does not meet — the two disagree on 7.3%
/// of uniform draws in (0,1). Using `s.ln()` here put every SEEDED gaussian
/// stream one ULP off HotSpot's: `new Random(42).nextGaussian()` printed
/// `1.141905315473055` against HotSpot's `1.1419053154730547`.
/// W7-44-numberformat-enum-and-double-tostring.md,
/// W7-54-strictmath-fdlibm-family-RETIRED-20260917.md.
///
/// Returns `None` if 64 consecutive pairs are rejected. P(reject) per pair is
/// `1 - pi/4 ~= 0.215`, so that is under `2^-42` — the cap exists only so a
/// pathological seed cannot hang the VM, and the JDK's own loop is unbounded.
fn rnd_gaussian_pair(next: &mut dyn FnMut(u32) -> i32) -> Option<(f64, f64)> {
    // One `nextDouble()` draw: `(next(26) << 27 + next(27)) * 2^-53`.
    fn uniform(next: &mut dyn FnMut(u32) -> i32) -> f64 {
        let hi = (next(26) as i64) << 27;
        let lo = next(27) as i64;
        (hi + lo) as f64 / ((1i64 << 53) as f64)
    }
    for _ in 0..64 {
        let v1 = 2.0 * uniform(next) - 1.0;
        let v2 = 2.0 * uniform(next) - 1.0;
        let s = v1 * v1 + v2 * v2;
        if s < 1.0 && s != 0.0 {
            let mult = (-2.0 * cratonvm_types::fdlibm::log(s) / s).sqrt();
            return Some((v1 * mult, v2 * mult));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// java.security.SecureRandom natives — OS-CSPRNG-backed
// ---------------------------------------------------------------------------

/// Shared body of `SecureRandom.<init>()V` and `SecureRandom.<init>([B)V`.
///
/// There is no per-instance RNG state to build — every draw pulls fresh
/// entropy from the OS (see the module header) — but the ctor is NOT
/// side-effect-free: the JDK's `getDefaultPRNG` records the selected
/// algorithm on the instance, and `SecureRandom.getAlgorithm()` is plain
/// bytecode reading that field (no native overrides it — grep `"getAlgorithm"`).
///
/// STUB-REMOVAL (wave 3): both ctors used to be pure no-ops, so `algorithm`
/// stayed null and `getAlgorithm()` handed back null — a caller doing
/// `sr.getAlgorithm().equals(…)` or logging it got an NPE instead of a name.
/// Record the same name the `getInstanceStrong()` factory already stamps via
/// `make_secure_random`, so every construction route agrees.
///
/// STUB-REMOVAL (jdk-wave2 L8): `provider` was the *other* field the real
/// `getDefaultPRNG` stamps, and it stayed null — so `getProvider()` (plain JDK
/// bytecode reading that field; no native overrides it) answered null on every
/// instance this module hands out. `regression-suite/src/RJdkSecurity.java:136`
/// asserts `sr.getProvider() != null` and failed in BOTH `--fast-boot` and
/// `--real-jdk`. Attach the owning `Provider` here as well.
fn secure_random_record_algorithm(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<(), MethodCallFailed> {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(());
    };
    // `create_string` can move the heap; pin `this` across it.
    let pin = ctx.pin_native_root(this);
    let algo = ctx.create_string(DEFAULT_ALGORITHM);
    let this = ctx.read_native_pin(pin, this);
    ctx.set_field_by_name(this, "algorithm", Value::Object(Some(algo)));
    ctx.unpin_native_roots(pin);
    secure_random_attach_provider(ctx, this, DEFAULT_ALGORITHM)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Algorithm names and the owning Provider
// ---------------------------------------------------------------------------

/// The algorithm name this module stamps on instances built by the plain
/// constructors and by `getInstanceStrong()` — every draw on those instances
/// reads the OS CSPRNG (see the module header), which is what the name says.
const DEFAULT_ALGORITHM: &str = "OS-CSPRNG";

/// The provider a stock JDK 25 registers `algo` under, or `None` when the name
/// is not one of the platform PRNGs at all.
///
/// Normalisation is alphanumeric-only + upper-case, matching
/// `jca::message_digest::algorithm_supported` and the JCA rule that algorithm
/// lookup is case-insensitive ("Windows-PRNG" → "WINDOWSPRNG").
fn secure_random_static_provider(algo: &str) -> Option<&'static str> {
    let normalised: String = algo
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    match normalised.as_str() {
        // SUN, JDK 9+: `DRBG` is the default and `SHA1PRNG` the legacy
        // algorithm — the two seeded into the service table by
        // `jca::provider_chain::seed_direct_native_engine_services`. The
        // `NativePRNG` family is registered by SUN on Unix only; accepting it
        // on every host is strictly closer to HotSpot than refusing a name
        // that is valid on the platform half the corpus runs on.
        "DRBG" | "SHA1PRNG" | "NATIVEPRNG" | "NATIVEPRNGBLOCKING" | "NATIVEPRNGNONBLOCKING" => {
            Some("SUN")
        }
        // SunMSCAPI — Windows only.
        "WINDOWSPRNG" => Some("SunMSCAPI"),
        // Our own name, stamped by the constructors and `getInstanceStrong()`.
        // A caller that round-trips `getAlgorithm()` back through
        // `getInstance` must not be refused by the check below.
        "OSCSPRNG" => Some("SUN"),
        _ => None,
    }
}

/// Whether `SecureRandom.getInstance(algo)` may succeed.
///
/// Real JDK resolves the name through the provider service map and throws
/// `NoSuchAlgorithmException` when nothing owns it. This module deliberately
/// bypasses that map (see the `getInstance` section below), and the bypass used
/// to fabricate an instance for *any* string — so
/// `SecureRandom.getInstance("NO-SUCH-PRNG")` quietly returned a working PRNG
/// where HotSpot raises (`regression-suite/src/RJdkSecurity.java:161`).
///
/// The static table is consulted first so the answer does not depend on the
/// service table having been seeded yet; a caller-registered provider
/// (`Security.addProvider` + `Provider.put("SecureRandom.<algo>", …)`) is
/// picked up by the second arm, which reads the very table `Security.getImpl`
/// consults.
fn secure_random_algorithm_supported(vm: usize, algo: &str) -> bool {
    secure_random_static_provider(algo).is_some()
        || crate::jca::provider_chain::find_service_provider_in(vm, "SecureRandom", algo).is_some()
}

/// Name to report from `SecureRandom.getProvider().getName()`. A provider that
/// actually claims the algorithm in the service table wins (that is what real
/// JDK's search order yields, including for user-registered providers); the
/// static table is the fallback.
fn secure_random_provider_name(vm: usize, algo: &str) -> String {
    if let Some(owner) =
        crate::jca::provider_chain::find_service_provider_in(vm, "SecureRandom", algo)
    {
        return owner;
    }
    secure_random_static_provider(algo)
        .unwrap_or("SUN")
        .to_string()
}

/// Populate the real `provider` field so `SecureRandom.getProvider()` answers a
/// live `java.security.Provider` rather than null.
///
/// Returns the (possibly GC-forwarded) receiver: materialising the Provider
/// allocates several objects, so the caller's `ObjectRef` can be stale on
/// return — `make_secure_random` hands its result straight back to Java, which
/// is exactly the shape that turns a missed re-read into a silent stale-oop.
fn secure_random_attach_provider(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    algo: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let name = secure_random_provider_name(ctx.vm_identity(), algo);
    let pin = ctx.pin_native_root(this);
    // `resolve_or_make_provider` hands back the caller's REAL registered
    // Provider when there is one, so `getProvider().getInfo()` et al. report
    // what that provider's own constructor set.
    // A fabricated provider gets its (empty) `Properties.map` in
    // `provider_chain::make_provider` (round 13 wave 10,
    // `CRATONVM_FABRICATED_PROVIDER_MAP`), which is what lets a `SecureRandom`
    // be serialized under `--compatible`.
    let provider = crate::jca::provider_chain::resolve_or_make_provider(ctx, &name);
    let this = ctx.read_native_pin(pin, this);
    ctx.set_field_by_name(this, "provider", Value::Object(Some(provider?)));
    ctx.unpin_native_roots(pin);
    Ok(this)
}

// ---------------------------------------------------------------------------
// SHA1PRNG — the one JCA algorithm whose output IS reproducible from a seed
// ---------------------------------------------------------------------------
//
// STUB-REMOVAL (wave 4). Every other method in this module reads the OS CSPRNG
// on every draw, and for `DRBG` / `NativePRNG` / `new SecureRandom()` that is
// both spec-legal and strictly stronger: their `engineSetSeed` mixes the
// caller's bytes with fresh entropy input, so two identically-seeded instances
// diverge on HotSpot too.
//
// `SHA1PRNG` is the exception, and the divergence was measurable rather than
// theoretical: `sun.security.provider.SecureRandom` keeps its whole state in a
// 20-byte SHA-1 digest, and `engineSetSeed` before the first draw makes the
// entire output stream a pure function of the seed. Two
// `SecureRandom.getInstance("SHA1PRNG")` instances seeded alike produce
// IDENTICAL bytes on HotSpot and produced DIFFERENT bytes here. Callers do
// depend on that (deterministic test fixtures, replayable data generation), so
// the faithful algorithm is implemented below rather than documented away.
//
// Scope, deliberately narrow — this engages ONLY when
//   (a) `getAlgorithm()` is literally "SHA1PRNG" (so `new SecureRandom()`,
//       `getInstanceStrong()` and every other algorithm are untouched), AND
//   (b) the caller supplied a seed via `setSeed`.
// An unseeded SHA1PRNG instance keeps drawing from the OS CSPRNG. That is not
// a fidelity loss: real SHA1PRNG self-seeds from `SeedGenerator` on first use,
// so its output is non-deterministic in exactly that case as well. The one
// residual difference is "draw first, then setSeed", where real JDK folds the
// pre-existing state into the digest and we start from `SHA1(seed)` — but the
// real state there came from OS entropy, so neither VM is reproducible.
//
// Note this makes CratonVM reproduce SHA1PRNG's weak-seed behaviour as well
// (`getInstance("SHA1PRNG")` + `setSeed(millis)` yields a predictable stream).
// That is what the algorithm IS, and what the same program does on HotSpot;
// diverging to "secretly stronger" hides the bug from the developer instead of
// from the attacker.
//
// `engineGenerateSeed` is deliberately NOT routed here — real SHA1PRNG serves
// it from `SeedGenerator` (the OS), not from the PRNG state, which is what
// `native_secure_random_generate_seed` already does.

const SHA1PRNG_DIGEST: usize = 20;

/// Per-instance `sun.security.provider.SecureRandom` state: the 20-byte
/// `state`, the unconsumed tail of the last digest block, and
/// `java.util.Random`'s cached second Gaussian deviate (SecureRandom does not
/// override `nextGaussian`, so the pairing is part of the reproducible stream).
#[derive(Clone)]
struct Sha1Prng {
    state: [u8; SHA1PRNG_DIGEST],
    remainder: [u8; SHA1PRNG_DIGEST],
    rem_count: usize,
    next_gaussian: Option<f64>,
}

/// Keyed exactly like `SEED_TABLE` (the instance's weak lock key) — see its
/// doc comment and the module doc for why, and why the state cannot live in a
/// field. Two seeded SHA1PRNG instances sharing one row (the identity-hash key
/// this had until gc-common w29-b) replayed one stream between them: a
/// `SecureRandom` whose output another instance's draws advance.
static SHA1PRNG_TABLE: RwLock<Option<FxHashMap<usize, Sha1Prng>>> = RwLock::new(None);

/// Fast bail for the overwhelmingly common case (nothing ever asked for a
/// seeded SHA1PRNG), so the draw paths do not take the table lock -- nor the
/// lock-key registry's -- per call.
static SHA1PRNG_ANY_SEEDED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn with_prng_write<R>(f: impl FnOnce(&mut FxHashMap<usize, Sha1Prng>) -> R) -> R {
    let mut g = SHA1PRNG_TABLE.write();
    if g.is_none() {
        *g = Some(FxHashMap::default());
    }
    f(g.as_mut().expect("table just initialized"))
}

fn secure_random_receiver(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// True when this instance was handed out by `getInstance("SHA1PRNG")`.
/// `algorithm` is the real field every construction route stamps (see
/// `secure_random_record_algorithm` / `make_secure_random`).
fn secure_random_is_sha1prng(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    match ctx.get_field_by_name(this, "algorithm") {
        Value::Object(Some(s)) => ctx
            .read_string(s)
            .is_some_and(|n| n.eq_ignore_ascii_case("SHA1PRNG")),
        _ => false,
    }
}

/// `sun.security.provider.SecureRandom.updateState` — state = state + output + 1
/// as a 160-bit little-endian addition, forced to change at least one bit.
fn sha1prng_update_state(state: &mut [u8; SHA1PRNG_DIGEST], output: &[u8; SHA1PRNG_DIGEST]) {
    let mut last: i32 = 1;
    let mut changed = false;
    for i in 0..SHA1PRNG_DIGEST {
        let v = state[i] as i32 + output[i] as i32 + last;
        let t = v as u8;
        changed |= state[i] != t;
        state[i] = t;
        last = v >> 8;
    }
    if !changed {
        state[0] = state[0].wrapping_add(1);
    }
}

/// `engineSetSeed(byte[])`: `digest.update(state); state = digest.digest(seed)`
/// — i.e. `SHA1(previous_state || seed)`, or `SHA1(seed)` on a fresh instance.
fn sha1prng_set_seed_bytes(ctx: &mut dyn NativeContext, this: ObjectRef, seed: &[u8]) {
    let key = random_state_key(ctx, this);
    let installed = with_prng_write(|t| {
        let mut input: Vec<u8> = Vec::with_capacity(SHA1PRNG_DIGEST + seed.len());
        if let Some(prev) = t.get(&key) {
            input.extend_from_slice(&prev.state);
        }
        input.extend_from_slice(seed);
        let digest = crate::real_sha1(&input);
        if digest.len() != SHA1PRNG_DIGEST {
            return false;
        }
        let mut state = [0u8; SHA1PRNG_DIGEST];
        state.copy_from_slice(&digest);
        t.insert(
            key,
            Sha1Prng {
                state,
                remainder: [0u8; SHA1PRNG_DIGEST],
                rem_count: 0,
                next_gaussian: None,
            },
        );
        true
    });
    if installed {
        SHA1PRNG_ANY_SEEDED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The key of `this`'s seeded SHA1PRNG state, or `None` -- every caller's
/// signal to fall through to the OS CSPRNG -- when nothing in the process was
/// ever seeded or `this` was never keyed. Looked up once per native call and
/// handed to the draw helpers below, so a multi-draw native (`nextLong`, the
/// rejection loops) takes the lock-key registry mutex once. Never mints.
fn sha1prng_key(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    if !SHA1PRNG_ANY_SEEDED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    existing_random_state_key(ctx, this)
}

/// `engineNextBytes` — returns false when the instance keyed `key` has no
/// seeded state, which is every caller's signal to fall through to the OS
/// CSPRNG.
fn sha1prng_next_bytes(key: usize, out: &mut [u8]) -> bool {
    with_prng_write(|t| {
        let Some(prng) = t.get_mut(&key) else {
            return false;
        };
        let mut index = 0usize;
        // Serve the tail of the previous digest block first.
        if prng.rem_count > 0 {
            let todo = (out.len() - index).min(SHA1PRNG_DIGEST - prng.rem_count);
            let mut r = prng.rem_count;
            for i in 0..todo {
                out[index + i] = prng.remainder[r];
                prng.remainder[r] = 0;
                r += 1;
            }
            prng.rem_count += todo;
            index += todo;
        }
        while index < out.len() {
            let digest = crate::real_sha1(&prng.state);
            if digest.len() != SHA1PRNG_DIGEST {
                return false;
            }
            let mut output = [0u8; SHA1PRNG_DIGEST];
            output.copy_from_slice(&digest);
            sha1prng_update_state(&mut prng.state, &output);
            let todo = (out.len() - index).min(SHA1PRNG_DIGEST);
            for b in output.iter_mut().take(todo) {
                out[index] = *b;
                index += 1;
                *b = 0;
            }
            prng.rem_count += todo;
            prng.remainder = output;
        }
        prng.rem_count %= SHA1PRNG_DIGEST;
        true
    })
}

/// `SecureRandom.next(int numBits)` — the protected override every inherited
/// `java.util.Random` method funnels through, so the derived
/// `nextInt`/`nextLong`/`nextDouble`/`nextFloat`/`nextBoolean` streams match
/// HotSpot bit for bit and not merely "deterministically".
fn sha1prng_next(key: usize, num_bits: u32) -> Option<i32> {
    let num_bytes = ((num_bits + 7) / 8) as usize;
    let mut buf = [0u8; 4];
    if !sha1prng_next_bytes(key, &mut buf[..num_bytes]) {
        return None;
    }
    let mut next: u32 = 0;
    for b in buf.iter().take(num_bytes) {
        next = (next << 8) | (*b as u32);
    }
    Some((next >> (num_bytes * 8 - num_bits as usize)) as i32)
}

/// `java.util.Random.nextDouble()` over the SHA1PRNG stream.
fn sha1prng_next_double(key: usize) -> Option<f64> {
    let hi = sha1prng_next(key, 26)? as i64;
    let lo = sha1prng_next(key, 27)? as i64;
    Some(((hi << 27) + lo) as f64 / ((1u64 << 53) as f64))
}

/// `java.util.Random.nextInt(bound)` over the SHA1PRNG stream — the legacy
/// power-of-two/rejection algorithm, which is what `Random` still specifies.
///
/// Works on the instance's key, not its reference (gc-common w29-b), so the
/// rejection loop holds no `ObjectRef` and needs no pin: nothing here can
/// collect, and the key is move-stable anyway.
fn sha1prng_next_int_bound(key: usize, bound: i32) -> Option<i32> {
    let m = bound - 1;
    let mut u = sha1prng_next(key, 31)?;
    if (bound & m) == 0 {
        return Some(((bound as i64).wrapping_mul(u as i64) >> 31) as i32);
    }
    loop {
        let r = u % bound;
        if u.wrapping_sub(r).wrapping_add(m) >= 0 {
            return Some(r);
        }
        u = sha1prng_next(key, 31)?;
    }
}

/// `java.util.Random.nextGaussian()` over the SHA1PRNG stream (Marsaglia polar,
/// including the cached partner deviate — dropping it would desynchronise the
/// stream from HotSpot's on the very next call).
fn sha1prng_next_gaussian(key: usize) -> Option<f64> {
    if let Some(v) = with_prng_write(|t| t.get_mut(&key).and_then(|p| p.next_gaussian.take())) {
        return Some(v);
    }
    // Bounded like `native_secure_random_next_gaussian`: P(reject) ≈ 0.215 per
    // pair, so 64 rounds is far below any practical failure probability.
    for _ in 0..64 {
        let v1 = 2.0 * sha1prng_next_double(key)? - 1.0;
        let v2 = 2.0 * sha1prng_next_double(key)? - 1.0;
        let s = v1 * v1 + v2 * v2;
        if s < 1.0 && s != 0.0 {
            // fdlibm `log`, not `f64::ln`: this stream is SEEDED and therefore
            // reproducible, so a last-ULP libm difference is observable as a
            // divergence from HotSpot. Same reason as `java.util.Random`'s
            // polar method — W7-44-numberformat-enum-and-double-tostring.md.
            let mult = (-2.0 * cratonvm_types::fdlibm::log(s) / s).sqrt();
            with_prng_write(|t| {
                if let Some(p) = t.get_mut(&key) {
                    p.next_gaussian = Some(v2 * mult);
                }
            });
            return Some(v1 * mult);
        }
    }
    None
}

pub fn native_secure_random_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Per the JDK SecureRandom contract the no-arg ctor selects a default
    // provider; we always select "OS-CSPRNG", the strongest source available.
    //
    // This body is registered for BOTH `<init>()V` and `<init>([B)V`, so the
    // null check is arity-gated: `new SecureRandom((byte[]) null)` NPEs on
    // HotSpot 25 (measured) — `SecureRandom.java:266` is
    // `Objects.requireNonNull(seed)`, reached before `getDefaultPRNG` can
    // discard the seed. `<init>()V` has no second argument and is unaffected.
    if args.len() >= 2 && matches!(args.get(1), Some(Value::Object(None))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    secure_random_record_algorithm(ctx, args)?;
    Ok(None)
}

/// `SecureRandom.setSeed(long)` — the real body is
/// `if (seed != 0) engineSetSeed(longToByteArray(seed))`, with the zero guard
/// present because `Random`'s constructor calls this virtually. Reproduced
/// exactly, including the little-endian long encoding.
///
/// For every algorithm other than SHA1PRNG this stays a no-op: their
/// `engineSetSeed` supplements a state we do not keep (each draw reads the OS
/// CSPRNG), and the spec forbids only WEAKENING the seed, which skipping the
/// supplement cannot do.
pub fn native_secure_random_set_seed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let seed = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => return Ok(None),
    };
    if seed == 0 {
        return Ok(None);
    }
    let Some(this) = secure_random_receiver(args) else {
        return Ok(None);
    };
    if !secure_random_is_sha1prng(ctx, this) {
        return Ok(None);
    }
    // `longToByteArray`: least-significant byte first.
    let mut bytes = [0u8; 8];
    let mut l = seed as u64;
    for b in bytes.iter_mut() {
        *b = l as u8;
        l >>= 8;
    }
    sha1prng_set_seed_bytes(ctx, this, &bytes);
    Ok(None)
}

/// `SecureRandom.setSeed(byte[])` — `engineSetSeed(seed)` verbatim for
/// SHA1PRNG; a no-op for the OS-CSPRNG-backed algorithms, for the reason on
/// `native_secure_random_set_seed` above.
pub fn native_secure_random_set_seed_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The null check precedes the receiver check: `setSeed(null)` NPEs on
    // HotSpot 25 (measured) regardless of algorithm, and the SHA1PRNG-only
    // early return below would otherwise swallow it for every other algorithm.
    // `SecureRandom.java:724` is `Objects.requireNonNull(seed)` — no message.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    let Some(this) = secure_random_receiver(args) else {
        return Ok(None);
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if !secure_random_is_sha1prng(ctx, this) {
        return Ok(None);
    }
    let len = ctx.array_length(arr);
    let mut seed: Vec<u8> = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            seed.push(b as u8);
        }
    }
    sha1prng_set_seed_bytes(ctx, this, &seed);
    Ok(None)
}

/// Does this receiver supply its own `nextBytes`?
///
/// `SecureRandom` overrides none of `nextInt`/`nextLong`/`nextBoolean`/
/// `nextFloat`/`nextDouble`/`nextGaussian`: it inherits `java.util.Random`'s,
/// and every one of them funnels through `SecureRandom.next(int)`, which is
/// `nextBytes(new byte[n])` — a VIRTUAL call. Serving them from the OS CSPRNG
/// is right for a plain `SecureRandom` and wrong for any subclass that supplies
/// its own bytes, because the override then never runs at all.
///
/// That is not a niche. Deterministic test doubles are built exactly this way
/// (bc-java's `FixedSecureRandom` replays a fixed stream through `nextBytes`),
/// and BouncyCastle's `ISO10126d2Padding` / `X923Padding` fill their padding
/// with `random.nextInt()` — so `DES/CBC/ISO10126Padding` drew its padding from
/// the OS CSPRNG instead of the caller's random and could not reproduce a
/// single known-answer vector (`BlockCipherTest`, index 6). Measured: HotSpot
/// makes six four-byte `nextBytes` draws through the caller's random where this
/// VM made none.
///
/// The walk stops at `java.security.SecureRandom` itself: its own `nextBytes`
/// is the one these natives are entitled to replace.
fn secure_random_bytes_overridden(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let mut class_id = ctx.class_id_of_object(this);
    loop {
        match ctx.class_name_arc_of_id(class_id).as_deref() {
            Some("java/security/SecureRandom") | None => return false,
            _ => {}
        }
        if ctx.class_declares_method(class_id, "nextBytes", "([B)V") {
            return true;
        }
        match ctx.superclass_of(class_id) {
            Some(parent) => class_id = parent,
            None => return false,
        }
    }
}

/// Hand an inherited draw on a [`secure_random_bytes_overridden`] receiver to
/// `java.util.Random`'s own body, with invokespecial semantics.
///
/// It used to re-dispatch on the receiver (`invoke_virtual_bytecode_only`).
/// That is the loop `crate::delegate_to_real_bytecode` documents: a subclass
/// that overrides `nextInt()` as `super.nextInt() ..` reaches this native
/// through its `invokespecial SecureRandom.nextInt`, and re-dispatching on the
/// receiver ran the override again, forever. `SecureRandom` declares none of
/// these draws (JDK 25), so the body the super-call names IS `Random`'s, and
/// for a subclass that does not override the draw the two resolutions agree.
/// Round 13 wave 9 (lane shadow3): the superclass body owns the call.
fn secure_random_subclass_draw(
    ctx: &mut dyn NativeContext,
    _this: ObjectRef,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    random_run_bytecode(ctx, method, descriptor, args)
}

pub fn native_secure_random_next_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A subclass that supplies its own bytes owns every derived value too;
    // see `secure_random_bytes_overridden`. Handing the call back to the
    // bytecode reaches `java.util.Random.nextInt` -> `SecureRandom.next(int)` ->
    // the override, which is what the JDK does and what this cannot fake.
    if let Some(this) = secure_random_receiver(args) {
        if secure_random_bytes_overridden(ctx, this) {
            return secure_random_subclass_draw(ctx, this, "nextInt", "()I", args);
        }
    }
    if let Some(key) = secure_random_receiver(args).and_then(|this| sha1prng_key(ctx, this)) {
        if let Some(v) = sha1prng_next(key, 32) {
            return Ok(Some(Value::Int(v)));
        }
    }
    // SECURITY FIX (V2): propagate entropy failure instead of silently
    // returning a predictable 0. Matches `native_secure_random_next_bytes`.
    let v = match os_random_u64() {
        Some(v) => v,
        None => {
            return Err(cratonvm_types::error::RuntimeError::SecurityException {
                message: "OS entropy source unavailable".to_string(),
            }
            .into());
        }
    };
    Ok(Some(Value::Int(v as i32)))
}

pub fn native_secure_random_next_int_bound(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A subclass that supplies its own bytes owns every derived value too;
    // see `secure_random_bytes_overridden`. Handing the call back to the
    // bytecode reaches `java.util.Random.nextInt` -> `SecureRandom.next(int)` ->
    // the override, which is what the JDK does and what this cannot fake.
    if let Some(this) = secure_random_receiver(args) {
        if secure_random_bytes_overridden(ctx, this) {
            return secure_random_subclass_draw(ctx, this, "nextInt", "(I)I", args);
        }
    }
    let bound = match args.get(1) {
        Some(Value::Int(b)) => *b,
        _ => 1,
    };
    if bound <= 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "bound must be positive".to_string(),
            }
            .into(),
        );
    }
    if let Some(key) = secure_random_receiver(args).and_then(|this| sha1prng_key(ctx, this)) {
        if let Some(v) = sha1prng_next_int_bound(key, bound) {
            return Ok(Some(Value::Int(v)));
        }
    }
    // Rejection sampling on full 32 bits to keep the distribution
    // unbiased for arbitrary bounds (JDK uses the same approach).
    let bound_u = bound as u32;
    loop {
        // SECURITY FIX (V2): propagate entropy failure instead of silently
        // drawing from a predictable 0. Matches `native_secure_random_next_bytes`.
        let entropy = match os_random_u64() {
            Some(v) => v,
            None => {
                return Err(cratonvm_types::error::RuntimeError::SecurityException {
                    message: "OS entropy source unavailable".to_string(),
                }
                .into());
            }
        };
        let raw = (entropy >> 32) as u32;
        // Largest multiple of `bound` that fits in u32.  `r < threshold`
        // → unbiased.  The expected number of rejections is < 1.
        let threshold = u32::MAX - (u32::MAX % bound_u);
        if raw < threshold {
            return Ok(Some(Value::Int((raw % bound_u) as i32)));
        }
    }
}

pub fn native_secure_random_next_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A subclass that supplies its own bytes owns every derived value too;
    // see `secure_random_bytes_overridden`. Handing the call back to the
    // bytecode reaches `java.util.Random.nextLong` -> `SecureRandom.next(int)` ->
    // the override, which is what the JDK does and what this cannot fake.
    if let Some(this) = secure_random_receiver(args) {
        if secure_random_bytes_overridden(ctx, this) {
            return secure_random_subclass_draw(ctx, this, "nextLong", "()J", args);
        }
    }
    if let Some(key) = secure_random_receiver(args).and_then(|this| sha1prng_key(ctx, this)) {
        if let Some(hi) = sha1prng_next(key, 32) {
            // `Random.nextLong()`: ((long)next(32) << 32) + next(32) — the low
            // half is sign-extended, exactly as the JDK's `+` does.
            //
            // SECURITY: the low half used to be `.unwrap_or(0)`. The high half
            // having succeeded makes a low-half failure near-impossible, but
            // "near-impossible" is not a property a random number generator
            // may rely on: the result would be a value whose bottom 32 bits are
            // a constant, returned as a `SecureRandom` draw with no signal.
            // Fall through to the OS entropy path below instead, which raises
            // if entropy is genuinely unavailable.
            if let Some(lo) = sha1prng_next(key, 32) {
                return Ok(Some(Value::Long(
                    ((hi as i64) << 32).wrapping_add(lo as i64),
                )));
            }
        }
    }
    // SECURITY FIX (V2): propagate entropy failure instead of silently
    // returning a predictable 0. Matches `native_secure_random_next_bytes`.
    let v = match os_random_u64() {
        Some(v) => v,
        None => {
            return Err(cratonvm_types::error::RuntimeError::SecurityException {
                message: "OS entropy source unavailable".to_string(),
            }
            .into());
        }
    };
    Ok(Some(Value::Long(v as i64)))
}

pub fn native_secure_random_next_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `SecureRandom.nextBytes(null)` NPEs on HotSpot 25 (measured), same as
        // the `java.util.Random` parent — see `native_random_next_bytes`. The
        // source is `Objects.requireNonNull(bytes)` with no message argument
        // (`SecureRandom.java:774`), so the message is genuinely null here and
        // `message: None` is the faithful answer, not a shortcut.
        Some(Value::Object(None)) => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,            }
            .into());        }
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr);
    if len == 0 {
        return Ok(None);
    }
    if let Some(key) = secure_random_receiver(args).and_then(|this| sha1prng_key(ctx, this)) {
        let mut seeded = vec![0u8; len];
        if sha1prng_next_bytes(key, &mut seeded) {
            for (i, b) in seeded.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
            }
            for b in seeded.iter_mut() {
                *b = 0;
            }
            return Ok(None);
        }
    }
    // Single bulk OS draw — much more efficient than per-byte LCG and
    // doesn't waste 56 bits/iteration like a u64-based loop would.
    let mut buf = vec![0u8; len];
    if !os_random_bytes(&mut buf) {
        // Don't silently return zeros.  Surface the failure as a
        // SecurityException so the caller sees the entropy source
        // is broken.
        return Err(cratonvm_types::error::RuntimeError::SecurityException {
            message: "OS entropy source unavailable".to_string(),
        }
        .into());
    }
    for (i, b) in buf.iter().enumerate() {
        // JVM signed-byte encoding.
        ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
    }
    // Defence in depth: clear the intermediate buffer.
    for b in buf.iter_mut() {
        *b = 0;
    }
    Ok(None)
}

pub fn native_secure_random_next_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A subclass that supplies its own bytes owns every derived value too;
    // see `secure_random_bytes_overridden`. Handing the call back to the
    // bytecode reaches `java.util.Random.nextDouble` -> `SecureRandom.next(int)` ->
    // the override, which is what the JDK does and what this cannot fake.
    if let Some(this) = secure_random_receiver(args) {
        if secure_random_bytes_overridden(ctx, this) {
            return secure_random_subclass_draw(ctx, this, "nextDouble", "()D", args);
        }
    }
    if let Some(key) = secure_random_receiver(args).and_then(|this| sha1prng_key(ctx, this)) {
        if let Some(d) = sha1prng_next_double(key) {
            return Ok(Some(Value::Double(d)));
        }
    }
    // 53 bits of entropy → IEEE-754 double in [0, 1).
    // SECURITY FIX (V2): propagate entropy failure instead of silently
    // returning a predictable 0.0. Matches `native_secure_random_next_bytes`.
    let v = match os_random_u64() {
        Some(v) => v,
        None => {
            return Err(cratonvm_types::error::RuntimeError::SecurityException {
                message: "OS entropy source unavailable".to_string(),
            }
            .into());
        }
    };
    let bits = v >> 11; // top 53 bits
    let d = bits as f64 / ((1u64 << 53) as f64);
    Ok(Some(Value::Double(d)))
}

pub fn native_secure_random_next_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A subclass that supplies its own bytes owns every derived value too;
    // see `secure_random_bytes_overridden`. Handing the call back to the
    // bytecode reaches `java.util.Random.nextBoolean` -> `SecureRandom.next(int)` ->
    // the override, which is what the JDK does and what this cannot fake.
    if let Some(this) = secure_random_receiver(args) {
        if secure_random_bytes_overridden(ctx, this) {
            return secure_random_subclass_draw(ctx, this, "nextBoolean", "()Z", args);
        }
    }
    if let Some(key) = secure_random_receiver(args).and_then(|this| sha1prng_key(ctx, this)) {
        if let Some(b) = sha1prng_next(key, 1) {
            return Ok(Some(Value::Int(i32::from(b != 0))));
        }
    }
    // SECURITY FIX (V2): propagate entropy failure instead of silently
    // returning a predictable false. Matches `native_secure_random_next_bytes`.
    let v = match os_random_u64() {
        Some(v) => v,
        None => {
            return Err(cratonvm_types::error::RuntimeError::SecurityException {
                message: "OS entropy source unavailable".to_string(),
            }
            .into());
        }
    };
    Ok(Some(Value::Int((v & 1) as i32)))}

pub fn native_secure_random_next_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A subclass that supplies its own bytes owns every derived value too;
    // see `secure_random_bytes_overridden`. Handing the call back to the
    // bytecode reaches `java.util.Random.nextFloat` -> `SecureRandom.next(int)` ->
    // the override, which is what the JDK does and what this cannot fake.
    if let Some(this) = secure_random_receiver(args) {
        if secure_random_bytes_overridden(ctx, this) {
            return secure_random_subclass_draw(ctx, this, "nextFloat", "()F", args);
        }
    }
    if let Some(key) = secure_random_receiver(args).and_then(|this| sha1prng_key(ctx, this)) {
        if let Some(b) = sha1prng_next(key, 24) {
            return Ok(Some(Value::Float(b as f32 / ((1u32 << 24) as f32))));
        }
    }
    // 24 bits → float in [0, 1).
    // SECURITY FIX (V2): propagate entropy failure instead of silently
    // returning a predictable 0.0. Matches `native_secure_random_next_bytes`.
    let v = match os_random_u64() {
        Some(v) => v,
        None => {
            return Err(cratonvm_types::error::RuntimeError::SecurityException {
                message: "OS entropy source unavailable".to_string(),
            }
            .into());
        }
    };
    let bits = (v >> 40) as u32; // top 24 bits
    let f = bits as f32 / ((1u32 << 24) as f32);
    Ok(Some(Value::Float(f)))
}

pub fn native_secure_random_next_gaussian(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A subclass that supplies its own bytes owns every derived value too;
    // see `secure_random_bytes_overridden`. Handing the call back to the
    // bytecode reaches `java.util.Random.nextGaussian` -> `SecureRandom.next(int)` ->
    // the override, which is what the JDK does and what this cannot fake.
    if let Some(this) = secure_random_receiver(args) {
        if secure_random_bytes_overridden(ctx, this) {
            return secure_random_subclass_draw(ctx, this, "nextGaussian", "()D", args);
        }
    }
    if let Some(key) = secure_random_receiver(args).and_then(|this| sha1prng_key(ctx, this)) {
        if let Some(g) = sha1prng_next_gaussian(key) {
            return Ok(Some(Value::Double(g)));
        }
    }
    // LOW-FIX: SecureRandom must NOT inherit java.util.Random's LCG-backed
    // nextGaussian (which would derive its two uniforms from the predictable
    // linear congruential generator on the synthetic seed field).  Instead we
    // draw both uniforms from the OS CSPRNG, matching the CSPRNG output of
    // every other SecureRandom method in this module.
    //
    // Algorithm: Marsaglia polar method — the same transform java.util.Random
    // .nextGaussian uses — but each uniform `v` in (-1, 1) is built from a
    // fresh 53-bit CSPRNG double rather than from `lcg_next`.  We generate one
    // deviate and discard the partner; SecureRandom keeps no per-instance state
    // here (consistent with the rest of the module), so caching the partner in
    // a field is neither needed nor possible.
    //
    // Cap the retry loop so a (vanishingly unlikely) run of rejected pairs
    // cannot hang us: P(reject) per pair ≈ 1 - π/4 ≈ 0.215, so 64 retries is
    // well under 2^-32 failure probability.
    #[inline]
    fn secure_uniform() -> Result<f64, cratonvm_types::error::RuntimeError> {
        // 53 bits of CSPRNG entropy → double in [0, 1); mapped to (-1, 1).
        match os_random_u64() {
            // Mirror `native_secure_random_next_double`: take the top 53 bits.
            Some(v) => Ok((v >> 11) as f64 / ((1u64 << 53) as f64)),
            None => Err(cratonvm_types::error::RuntimeError::SecurityException {
                message: "OS entropy source unavailable".to_string(),
            }),
        }
    }
    for _ in 0..64 {
        let v1 = 2.0 * secure_uniform()? - 1.0;
        let v2 = 2.0 * secure_uniform()? - 1.0;
        let s = v1 * v1 + v2 * v2;
        if s < 1.0 && s != 0.0 {
            // fdlibm `log` for the same reason as the seeded paths, though
            // here the uniforms come from the OS CSPRNG so no observer can
            // tell: kept identical so the three polar sites cannot drift.
            let mult = (-2.0 * cratonvm_types::fdlibm::log(s) / s).sqrt();
            return Ok(Some(Value::Double(v1 * mult)));
        }
    }
    // Extraordinarily unlikely with a healthy CSPRNG; surface loud rather than
    // returning a misleading 0.0.
    Err(cratonvm_types::error::RuntimeError::SecurityException {
        message: "SecureRandom.nextGaussian: polar method failed to converge".to_string(),
    }
    .into())
}

pub fn native_secure_random_generate_seed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if n < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                // `SecureRandom.java:878` verbatim. "must be non-negative" is
                // `RandomSupport.BAD_SIZE`, which is a DIFFERENT method's
                // message (`ints`/`longs`/`doubles` stream size).
                message: "numBytes cannot be negative".to_string(),
            }
            .into(),
        );
    }
    let n = n as usize;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, n);
    if n == 0 {
        return Ok(Some(Value::Object(Some(arr))));
    }
    let mut buf = vec![0u8; n];
    if !os_random_bytes(&mut buf) {
        return Err(cratonvm_types::error::RuntimeError::SecurityException {
            message: "OS entropy source unavailable".to_string(),
        }
        .into());
    }
    for (i, b) in buf.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// java.security.SecureRandom.getInstance(...) — static-factory interception
// ---------------------------------------------------------------------------
//
// The real-JDK `SecureRandom.getInstance(algorithm)` body routes through
// `sun.security.jca.GetInstance.getInstance("SecureRandom", SecureRandomSpi
// .class, algorithm)` → the provider service map.  We deliberately never
// register a `SecureRandom` service entry (the whole module bypasses the
// provider machinery — see the header and `register_random_and_securerandom_
// natives`), so that path dead-ends in `provider_chain::getinstance_instance_
// search` with `not implemented: no SecureRandom SHA1PRNG implementation in any
// provider`, which hard-stops apps such as H2's `org.h2.test.TestAll`.
//
// Mirror `jca::message_digest::md_get_instance`: intercept the static factory
// directly and hand back a genuine `java.security.SecureRandom` whose instance
// methods are already overridden onto the OS CSPRNG.  The requested algorithm
// name is recorded in the real `algorithm` field so `getAlgorithm()` reports it
// faithfully; the byte stream itself is OS-CSPRNG (strictly stronger than
// SHA1PRNG), consistent with this module's "always select the strongest source"
// policy.  Allocating without running the real `<init>` also avoids the
// `getDefaultPRNG` → `Providers.getProviderList()` NPE that the no-arg ctor
// native already sidesteps.

/// Allocate a `java.security.SecureRandom` and record `algorithm` by name.
/// The String is created and pinned *before* the object allocation so a moving
/// GC during `alloc_concurrent_synthetic` cannot leave us writing through a
/// stale reference.
fn make_secure_random(
    ctx: &mut dyn NativeContext,
    algorithm: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let algo_str = ctx.create_string(algorithm);
    let pin = ctx.pin_native_root(algo_str);
    let sr = crate::try_alloc_concurrent_synthetic(ctx, "java/security/SecureRandom", 4)?;
    let algo_str = ctx.read_native_pin(pin, algo_str);
    // Resolve `algorithm:String` by name so the slot matches the real layout
    // regardless of synthetic vs real-JDK field ordering.
    ctx.set_field_by_name(sr, "algorithm", Value::Object(Some(algo_str)));
    ctx.unpin_native_roots(pin);
    // `provider` is the second field `getDefaultPRNG` / `GetInstance` stamp;
    // without it `getProvider()` reads back null. Returns the forwarded `sr`.
    Ok(secure_random_attach_provider(ctx, sr, algorithm)?)
}

/// The provider named by the second argument of a two-argument `getInstance`
/// (the `String` or the `Provider` overload), or `None` when it is null.
fn secure_random_named_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<String> {
    let Some(Value::Object(Some(p))) = args.get(1).copied() else {
        return None;
    };
    let is_string = ctx
        .class_name_of_id(ctx.class_id_of_object(p))
        .is_some_and(|n| n == "java/lang/String");
    if is_string {
        ctx.read_string(p)
    } else {
        match ctx.invoke_virtual(p, "getName", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
            _ => None,
        }
    }
}

/// Names of the providers on the JDK's own provider list, in chain order.
fn secure_random_installed_provider_names(ctx: &mut dyn NativeContext) -> Vec<String> {
    let arr = match ctx.invoke_static(
        "java/security/Security",
        "getProviders",
        "()[Ljava/security/Provider;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => return Vec::new(),
    };
    let pin = ctx.pin_native_root(arr);
    let len = ctx.array_length(arr);
    let mut names = Vec::with_capacity(len);
    for i in 0..len {
        let arr = ctx.read_native_pin(pin, arr);
        if let Value::Object(Some(p)) = ctx.get_array_element(arr, i) {
            if let Ok(Some(Value::Object(Some(s)))) =
                ctx.invoke_virtual(p, "getName", "()Ljava/lang/String;", &[])
            {
                if let Some(n) = ctx.read_string(s) {
                    names.push(n);
                }
            }
        }
    }
    ctx.unpin_native_roots(pin);
    names
}

/// Run the JDK's own `SecureRandom.getInstance` body for `args`'s overload,
/// skipping this module's OS-CSPRNG factory. It resolves through the real
/// provider list, which is the only thing that knows a provider an application
/// installed.
fn secure_random_run_real_get_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let descriptor = match args.len() {
        1 => "(Ljava/lang/String;)Ljava/security/SecureRandom;",
        _ => {
            let is_string = match args.get(1) {
                Some(Value::Object(Some(p))) => ctx
                    .class_name_of_id(ctx.class_id_of_object(*p))
                    .is_some_and(|n| n == "java/lang/String"),
                _ => true,
            };
            if is_string {
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/SecureRandom;"
            } else {
                "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/SecureRandom;"
            }
        }
    };
    ctx.invoke_special_bytecode_only("java/security/SecureRandom", "getInstance", descriptor, args)
}

/// `SecureRandom.getInstance(String algorithm)` — static factory.  `algorithm`
/// is the first parameter (static method: no `this` in `args`).
pub fn native_secure_random_get_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Two different answers, not one. `getInstance(null)` is
    // `Objects.requireNonNull(algorithm, "null algorithm name")`
    // (`SecureRandom.java:391`) — a NullPointerException. `getInstance("")` is
    // a NoSuchAlgorithmException whose message is `" SecureRandom not
    // available"`, which the `secure_random_algorithm_supported` branch below
    // already produces for the empty string. Both were collapsed into one
    // IllegalArgumentException, so the type was wrong for null and the type and
    // the wording were wrong for "". Measured on OpenJDK 25.0.3+9.
    let algo = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        Some(Value::Object(None)) | None => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("null algorithm name".to_string()),
            }
            .into());
        }
        _ => String::new(),
    };
    // Real JDK dead-ends an unknown name in `GetInstance` with
    // `NoSuchAlgorithmException("<algo> SecureRandom not available")`. Because
    // this native bypasses the provider search entirely it used to fabricate a
    // working PRNG for every string, so ordinary probing code
    // (`try { getInstance(x) } catch (NoSuchAlgorithmException e) { fallback }`)
    // never took its fallback and an outright typo went undetected. Mirror the
    // real contract, including the message wording.
    //
    // A third-party provider on the chain changes the answer: the chain is
    // searched in order, so one installed ahead of SUN (`Security.
    // insertProviderAt(new BouncyCastleProvider(), 1)`) owns the names it
    // registers, and a name only it offers (`DEFAULT`, which bc-jsse asks for)
    // is not "not available". The platform-PRNG shortcut below knows neither,
    // so hand those lookups to the JDK body, which walks the real provider list.
    if args.len() == 1 {
        let vm = ctx.vm_identity();
        let owner_is_third_party =
            crate::jca::provider_chain::find_service_provider_in(vm, "SecureRandom", &algo)
                .is_some_and(|o| !crate::jca::provider_chain::is_natively_serviced(&o));
        // The chain registry is only told about providers installed through the
        // natively handled `Security` entry points; an application's own
        // `Security.addProvider` lands in the JDK's provider list, so read that.
        let installed = secure_random_installed_provider_names(ctx);
        let before_sun = installed
            .iter()
            .take_while(|n| n.as_str() != "SUN")
            .any(|n| !crate::jca::provider_chain::is_natively_serviced(n));
        let any_third_party = installed
            .iter()
            .any(|n| !crate::jca::provider_chain::is_natively_serviced(n));
        let unknown_with_third_party =
            !secure_random_algorithm_supported(vm, &algo) && any_third_party;
        if owner_is_third_party || before_sun || unknown_with_third_party {
            return secure_random_run_real_get_instance(ctx, args);
        }
    }
    if !secure_random_algorithm_supported(ctx.vm_identity(), &algo) {
        return Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
            ctx,
            &format!("{algo} SecureRandom not available"),
        ));
    }
    Ok(Some(Value::Object(Some(make_secure_random(ctx, &algo)?))))
}

/// `SecureRandom.getInstance(String algorithm, String provider)` and
/// `SecureRandom.getInstance(String algorithm, Provider provider)` — the
/// provider argument is ignored (every algorithm resolves to the OS CSPRNG),
/// so both overloads read `algorithm` from slot 0 exactly like the 1-arg form.
pub fn native_secure_random_get_instance_with_provider(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // ORDER IS OBSERVABLE, and it was backwards. All three 2-arg overloads
    // OPEN with `Objects.requireNonNull(algorithm, "null algorithm name")`
    // (`SecureRandom.java:439` for the `String` provider, `:481` for the
    // `Provider` one) — before any provider resolution. So a null algorithm
    // beats a bad provider, and the provider is resolved first only among
    // NON-null algorithm names. Measured on OpenJDK 25.0.3+9:
    //
    //   getInstance(null, "SUN")           -> NPE "null algorithm name"
    //   getInstance(null, "NOPE")          -> NPE "null algorithm name"
    //   getInstance(null, (String) null)   -> NPE "null algorithm name"
    //   getInstance(null, (Provider) null) -> NPE "null algorithm name"
    //   getInstance("SHA1PRNG", (String) null) -> IAE "missing provider"
    //
    // Running the provider checks first answered rows 2–4 with
    // NoSuchProviderException / IllegalArgumentException instead. Hoisting the
    // null-algorithm check here rather than relying on the 1-arg body it
    // delegates to is the whole fix: that body is reached only AFTER both
    // provider checks have already had their chance to throw.
    if matches!(args.first(), Some(Value::Object(None)) | None) {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some("null algorithm name".to_string()),
        }
        .into());
    }
    if let Some(named) = secure_random_named_provider(ctx, args) {
        if !crate::jca::provider_chain::is_natively_serviced(&named) {
            return secure_random_run_real_get_instance(ctx, args);
        }
    }
    // The provider argument is otherwise discarded (every algorithm here is
    // served by the OS CSPRNG regardless), but real JDK does resolve the named
    // provider before looking the algorithm up, and rejects one that was never
    // registered — see `check_named_provider_arg`.
    crate::jca::provider_chain::check_named_provider_arg(
        ctx,
        args,
        1,
        crate::jca::provider_chain::ProviderArgWording::Shared,
    )?;
    let algorithm = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    crate::jca::provider_chain::check_provider_ownership(
        ctx,
        args,
        1,
        "SecureRandom",
        &algorithm,
        crate::jca::provider_chain::ProviderArgWording::Shared,
    )?;
    native_secure_random_get_instance(ctx, args)
}

/// `SecureRandom.getInstanceStrong()` — the JDK consults the
/// `securerandom.strongAlgorithms` security property and tries each entry. Our
/// OS CSPRNG is already the strongest source, so return it directly instead of
/// driving the (bypassed) provider machinery.
pub fn native_secure_random_get_instance_strong(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(make_secure_random(
        ctx,
        DEFAULT_ALGORITHM,
    )?))))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Use the real JDK's Random bytecode by default. The compiled callee and
/// boxing paths now preserve the intrinsics this used to lose: on the
/// 2026-10-03 Azure baseline, seeded construction/draw fell from 2208 to
/// 171 ns/op and shared draws from 238 to 47 ns/op. Both runtime modes
/// match HotSpot, including SecureRandom superclass construction.
/// `CRATONVM_JDK_RANDOM=0` restores the native shadow for comparisons.
/// Synthetic-JDK mode always keeps the native implementation.
/// See docs/feature-designs/native-dispatch-architecture.md
/// jpalargeblobtest-per-native-call-floor-20260830.md.
fn jdk_random_enabled() -> bool {
    true}

#[cfg(test)]
mod random_registration_policy_tests;

/// Register all `java.util.Random` and `java.security.SecureRandom`
/// natives.  Must be called AFTER `register_security_natives` so the
/// deterministic LCG-based handlers override the legacy CSPRNG aliases
/// that older code in `lib.rs` registers under `java/util/Random`.
pub fn register_random_and_securerandom_natives(registry: &mut NativeMethodRegistry) {
    // These are real, spec-exact handlers (deterministic LCG for `Random`,
    // OS-CSPRNG for `SecureRandom`), NOT synthetic stubs. Register them under
    // `Intrinsic` so the strict no-stubs build (`drop_synthetic_stubs`) does NOT
    // drop them: `NativeMethodRegistry::register` early-returns when the current
    // category is `SyntheticStub`, and this function inherits the caller's
    // category — which at one call site was `SyntheticStub`, silently dropping
    // every `java/util/Random` registration so the LCG never ran and seeded
    // `Random` returned all-zero output.
            // --- java.util.Random ---
    //
    // Serve EXACT `java.util.Random` receivers only: every body below hands a
    // proper subclass to `Random`'s own bytecode (`random_native_key`,
    // `random_ctor_runs_bytecode`; `CRATONVM_RANDOM_SUBCLASS_YIELD`), so these
    // are not leaf natives -- a subclass receiver re-enters Java.
    //
    // The real JDK owns Random's state in its AtomicLong field and handles
    // subclass dispatch and serialization. Retiring this shadow also avoids
    // a weak-key allocation and two global table writes per constructor.
    // SecureRandom keeps its separate provider handlers below; synthetic-JDK
    // mode has no classfile implementation and still needs these handlers.
    if !registry.real_jdk() || !jdk_random_enabled() {
        let r = "java/util/Random";
        registry.register(r, "<init>", "()V", native_random_init_noseed, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "<init>", "(J)V", native_random_init_seed, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "setSeed", "(J)V", native_random_set_seed, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "nextInt", "()I", native_random_next_int, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "nextInt", "(I)I", native_random_next_int_bound, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "nextLong", "()J", native_random_next_long, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "nextDouble", "()D", native_random_next_double, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "nextFloat", "()F", native_random_next_float, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "nextBoolean", "()Z", native_random_next_boolean, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "nextBytes", "([B)V", native_random_next_bytes, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(r, "nextGaussian", "()D", native_random_next_gaussian, cratonvm_native_api::NativeKind::Intrinsic);
        // Round 13 wave 10 (lane misc10): the real `writeObject` reads fields
        // the natives above never write; this one writes them first and then
        // runs the real body (`CRATONVM_RANDOM_SERIAL_STATE`). Real JDK only:
        // the synthetic `Random` has no body to run.
        if registry.real_jdk() {
            registry.register(
                r,
                "writeObject",
                "(Ljava/io/ObjectOutputStream;)V",
                native_random_write_object, cratonvm_native_api::NativeKind::Intrinsic,
            );
        }
    }

    // --- java.security.SecureRandom ---
    //
    // SecureRandom extends Random so the unqualified inherited methods
    // (nextInt with no args, nextDouble, etc.) dispatch to the parent
    // class's natives unless we override them here.  We override every
    // public method to pull fresh OS entropy on each call — that is
    // SecureRandom's contract and the user must not get LCG output
    // when they ask for cryptographic randomness.
    let sr = "java/security/SecureRandom";
    registry.register(sr, "<init>", "()V", native_secure_random_init, cratonvm_native_api::NativeKind::Intrinsic);
    // `SecureRandom(byte[] seed)`: the seed argument is DISCARDED, and that
    // matches HotSpot rather than merely being convenient. The JDK ctor selects
    // the DEFAULT algorithm — DRBG on JDK 9+, never SHA1PRNG — and delivers the
    // seed through `getDefaultPRNG(true, seed)` → `engineSetSeed`, which for
    // DRBG reseeds with fresh entropy input. So `new SecureRandom(seed)` is not
    // reproducible on HotSpot either, and the seeded-SHA1PRNG path implemented
    // above is deliberately NOT reached from here (this ctor records the
    // algorithm as "OS-CSPRNG", not "SHA1PRNG"). The ctor is not a no-op: it
    // records `algorithm` like the no-arg form.
    //
    // NOTE for callers porting tests: reproducible replay is available exactly
    // where the JDK guarantees it — `java.util.Random` (LCG, above) and
    // `SecureRandom.getInstance("SHA1PRNG")` + `setSeed` (below).
    registry.register(sr, "<init>", "([B)V", native_secure_random_init, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(sr, "setSeed", "(J)V", native_secure_random_set_seed, cratonvm_native_api::NativeKind::Intrinsic);
    // STUB-REMOVAL (wave 4): was a no-op, justified by "engineSetSeed only
    // SUPPLEMENTS the seed and we keep no PRNG state to supplement". True for
    // DRBG/NativePRNG/`new SecureRandom()`, and FALSE for SHA1PRNG, whose
    // entire state is the seed digest — `getInstance("SHA1PRNG")` seeded twice
    // alike yields identical bytes on HotSpot and yielded different bytes here.
    // Now routed through the real `sun.security.provider.SecureRandom`
    // algorithm for that one case; see the SHA1PRNG section above for the exact
    // scope and for why every other algorithm stays on the OS CSPRNG.
    registry.register(sr, "setSeed", "([B)V", native_secure_random_set_seed_bytes, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(sr, "nextInt", "()I", native_secure_random_next_int, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(sr, "nextInt", "(I)I", native_secure_random_next_int_bound, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(sr, "nextLong", "()J", native_secure_random_next_long, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(sr, "nextBytes", "([B)V", native_secure_random_next_bytes, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(sr, "nextDouble", "()D", native_secure_random_next_double, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(sr, "nextBoolean", "()Z", native_secure_random_next_boolean, cratonvm_native_api::NativeKind::Intrinsic);
    registry.register(sr, "nextFloat", "()F", native_secure_random_next_float, cratonvm_native_api::NativeKind::Intrinsic);
    // LOW-FIX: override nextGaussian on SecureRandom so it derives its uniforms
    // from the CSPRNG; otherwise it inherits java.util.Random's LCG-backed
    // `native_random_next_gaussian` (registered above on java/util/Random),
    // leaking predictable Gaussian deviates from a "secure" source.
    registry.register(
        sr,
        "nextGaussian",
        "()D",
        native_secure_random_next_gaussian, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        sr,
        "generateSeed",
        "(I)[B",
        native_secure_random_generate_seed, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Static factories — `getInstance(...)` / `getInstanceStrong()`.  Without
    // these the real-JDK body falls through to the JCA provider chain, which has
    // no `SecureRandom` service entry and dead-ends in "no SecureRandom <algo>
    // implementation in any provider" (the H2 `TestAll` hard stop).  Intercept
    // them so any requested algorithm (SHA1PRNG, DRBG, NativePRNG, …) yields an
    // OS-CSPRNG-backed instance.
    registry.register(
        sr,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/SecureRandom;",
        native_secure_random_get_instance, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        sr,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/SecureRandom;",
        native_secure_random_get_instance_with_provider, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        sr,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/SecureRandom;",
        native_secure_random_get_instance_with_provider, cratonvm_native_api::NativeKind::Intrinsic,
    );
    registry.register(
        sr,
        "getInstanceStrong",
        "()Ljava/security/SecureRandom;",
        native_secure_random_get_instance_strong, cratonvm_native_api::NativeKind::Intrinsic,
    );
    
    // `sun.security.provider.NativeSeedGenerator.nativeGenerateSeed(byte[])Z`:
    // the Windows build of that class declares it `native` (a CryptoAPI
    // seed call in HotSpot's `libjava`), so no bytecode exists and no
    // `--real-jdk` run can avoid it. Nothing registered it, which made every
    // `SeedGenerator` reached through the real JDK bytecode (for example
    // `SecureRandom.generateSeed`, the DRBG seed path, once the Java-level
    // bridges above it are not in front) die with an `UnsatisfiedLinkError`
    // (SBR2 wave 9 PUREDOORS census, family F3). The OS CSPRNG fills the
    // array. A `Bridge` by explicit category: an ambient `SyntheticStub`
    // would be dropped under `--real-jdk`, which is the failure again.
            registry.register(
        "sun/security/provider/NativeSeedGenerator",
        "nativeGenerateSeed",
        "([B)Z",
        native_seed_generator_native_generate_seed, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 3, and `common-w28b-identity-hash-eviction-channel-is-vm-less-and-zgc-only`).
///
/// The mock's identity hash is the address truncated to `i32` (lesson qq), so
/// two addresses 4 GiB apart share a hash, as two live objects do in a real
/// heap once its identity-hash counter has wrapped; and one address seen from
/// two VM identities is the cross-VM collision (every heap numbers its hashes
/// from the same seed). Never dereferenced: the Random natives touch only the
/// key. Every test owns its VM identities (`0xB29_0xxx`) and forgets them in a
/// guard, since the lock-key registry and these tables are process-wide.
#[cfg(test)]
mod w29b_random_state_key_tests;

/// Round 13 wave 10 (lane misc10): the side table and the real fields meet at
/// serialization (`r13w9-shadow3-random-serialization-ignores-the-side-table-FIXED-20260928.md`).
/// The mock models the real JDK layout of `Random` (`seed`, `nextNextGaussian`,
/// `haveNextNextGaussian`) and `AtomicLong` (`value`) through declared fields.
#[cfg(test)]
mod r13w10_random_serial_state_tests;

/// `NativeSeedGenerator.nativeGenerateSeed(byte[] result)`: fill `result` from
/// the OS entropy source; `true` when it did, as the Windows `libjava` call
/// answers.
fn native_seed_generator_native_generate_seed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
    };
    let n = ctx.array_length(arr);
    let mut buf = vec![0u8; n];
    let ok = n == 0 || os_random_bytes(&mut buf);
    if ok && n > 0 {
        ctx.write_byte_array_from(arr, 0, &buf);
    }
    Ok(Some(Value::Int(i32::from(ok))))
}
