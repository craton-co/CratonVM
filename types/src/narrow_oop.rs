// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-wide narrow-oop (compressed object pointer) configuration.
//!
//! `cratonvm_gc::compressed_oops` owns the *policy* (mode selection, heap
//! geometry, klass compression). This module owns the *mechanism* that the hot
//! paths need: a base/shift pair published once at VM init and read on every
//! reference field load/store.
//!
//! It lives in `cratonvm-types` rather than `cratonvm-gc` because the compact
//! field accessors ([`crate::read_compact_field`] /
//! [`crate::write_compact_field`]) and the array-element helpers are defined
//! here and are below `cratonvm-gc` in the dependency graph.
//!
//! # Contract
//!
//! * [`enable`] is called **exactly once**, at VM init, before any object is
//!   allocated and before any class layout is registered. After that the
//!   configuration is immutable for the lifetime of the process — an object
//!   must never be read back under a different base/shift than it was written.
//! * When disabled (the default) every accessor is a raw 64-bit pointer and
//!   [`ref_field_size`] / [`ref_element_size`] return 8, exactly reproducing
//!   the pre-existing layout byte for byte.
//! * When enabled, a reference slot is 4 bytes holding `(addr - base) >> shift`,
//!   with 0 reserved for null.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Narrow reference slot width in bytes when compression is active.
pub const NARROW_REF_SIZE: usize = 4;
/// Wide (uncompressed) reference slot width in bytes.
pub const WIDE_REF_SIZE: usize = 8;

static ENABLED: AtomicBool = AtomicBool::new(false);
static BASE: AtomicU64 = AtomicU64::new(0);
static SHIFT: AtomicUsize = AtomicUsize::new(0);
/// Exclusive upper bound of the encodable address range.
static LIMIT: AtomicU64 = AtomicU64::new(0);

/// Whether narrow oops are active for this process.
#[inline(always)]
pub fn narrow_oops_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Active heap base. Meaningless unless [`narrow_oops_enabled`].
#[inline(always)]
pub fn narrow_base() -> u64 {
    BASE.load(Ordering::Relaxed)
}

/// Active shift. Meaningless unless [`narrow_oops_enabled`].
#[inline(always)]
pub fn narrow_shift() -> usize {
    SHIFT.load(Ordering::Relaxed)
}

/// Exclusive upper bound of the encodable range.
#[inline(always)]
pub fn narrow_limit() -> u64 {
    LIMIT.load(Ordering::Relaxed)
}

/// Byte width of a compact reference *instance field*.
#[inline(always)]
pub fn ref_field_size() -> usize {
    if narrow_oops_enabled() {
        NARROW_REF_SIZE
    } else {
        WIDE_REF_SIZE
    }
}

/// Byte width of a reference *array element*.
#[inline(always)]
pub fn ref_element_size() -> usize {
    ref_field_size()
}

/// Enable narrow oops with the supplied geometry.
///
/// `base` must be below every object address the heap will ever produce and
/// `base + (u32::MAX << shift)` must be above every such address. Returns
/// `false` (leaving compression disabled) if the geometry is unusable, so the
/// caller can fall back to wide pointers instead of corrupting the heap.
pub fn enable(base: u64, shift: usize) -> bool {
    if shift > 3 {
        return false;
    }
    // Reserve encoding 0 for null: no live object may sit exactly at `base`.
    // Callers push `base` below the first allocatable byte to guarantee this.
    let span = ((u32::MAX as u64) << shift) as u128;
    let Some(limit) = (base as u128).checked_add(span) else {
        return false;
    };
    if limit > u64::MAX as u128 {
        return false;
    }
    BASE.store(base, Ordering::Relaxed);
    SHIFT.store(shift, Ordering::Relaxed);
    LIMIT.store(limit as u64, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Release);
    true
}

/// Disable narrow oops. Test-only: the production contract is write-once.
///
/// # This is not a knob a test may flip while other tests run
///
/// `ENABLED` is PROCESS-global and is read on the hot path of every reference
/// access — [`ref_element_size`] is what `element_byte_size(Reference)`
/// returns, so flipping it retypes every reference array in the process from
/// an 8-byte element stride to a 4-byte one, mid-life. A `cargo test` binary
/// runs its tests on parallel threads in ONE process, so a test that enables
/// narrow oops around its own assertions silently reinterprets the heaps of
/// every test running beside it: an array written with 8-byte elements is read
/// back at stride 4, and element `2k+1` decodes the HIGH half of pointer `k`
/// as if it were a whole reference.
///
/// That is not hypothetical. It is the root cause of the long-standing
/// `g1::tests::parallel_matches_serial_no_loss_or_dup` flake — which read as a
/// race in G1's parallel evacuator for months (`audits/g1-audit.md` G1-9, in
/// the internal record tree) and is not one — and of the sibling
/// `compressed_oops::assert_region_encodable` flake in the `gen_heap` tests,
/// whose reported window `0x20000000..0x81ffffff8` is exactly the `BASE`/`SHIFT`
/// a narrow-oop unit test installs.
///
/// A local `Mutex` that serialises the narrow-oop tests against EACH OTHER does
/// not help: the blast radius is every other test in the binary, and none of
/// them takes that lock. Use [`encode_with`] / [`decode_with`] and pass the
/// geometry explicitly instead.
pub fn disable_for_test() {
    ENABLED.store(false, Ordering::Release);
    BASE.store(0, Ordering::Relaxed);
    SHIFT.store(0, Ordering::Relaxed);
    LIMIT.store(0, Ordering::Relaxed);
}

/// Whether `addr` can be represented as a narrow oop under the active config.
#[inline]
pub fn is_encodable(addr: u64) -> bool {
    if addr == 0 {
        return true;
    }
    let base = narrow_base();
    let shift = narrow_shift();
    if addr <= base || addr >= narrow_limit() {
        return false;
    }
    ((addr - base) & ((1u64 << shift) - 1)) == 0
}

// --- Out-of-range accounting -------------------------------------------------

static OOR_HITS: AtomicUsize = AtomicUsize::new(0);

/// Number of addresses that failed the encodability guard so far. A non-zero
/// value means the heap geometry chosen at init was wrong; the run is not
/// trustworthy.
pub fn out_of_range_hits() -> usize {
    OOR_HITS.load(Ordering::Relaxed)
}

#[cold]
#[inline(never)]
fn report_unencodable(addr: u64) -> u32 {
    let n = OOR_HITS.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        eprintln!(
            "cratonvm: FATAL narrow-oop encode failure: address {addr:#x} outside \
             [{:#x}, {:#x}) shift={} - compressed oops geometry is wrong",
            narrow_base(),
            narrow_limit(),
            narrow_shift(),
        );
    }
    // Fail loudly rather than silently truncating into a wrong-but-plausible
    // narrow oop, which would be undetectable heap corruption.
    panic!("narrow-oop encode failure for address {addr:#x}");
}

/// Encode a 64-bit heap address into its 32-bit narrow form.
///
/// Only valid while [`narrow_oops_enabled`]. `0` maps to `0`.
#[inline(always)]
pub fn encode(addr: u64) -> u32 {
    encode_with(narrow_base(), narrow_shift(), addr)
}

/// Decode a 32-bit narrow oop back to a 64-bit address. `0` maps to `0`.
#[inline(always)]
pub fn decode(narrow: u32) -> u64 {
    decode_with(narrow_base(), narrow_shift(), narrow)
}

/// [`encode`] against an EXPLICIT geometry instead of the published globals.
///
/// The transform is a pure function of `(base, shift, addr)`; the globals are
/// only how production names the one geometry a live heap was allocated under.
/// Tests that need to exercise the encoding must use this, NOT [`enable`] —
/// see the warning on [`disable_for_test`] for why flipping the globals under
/// a running test binary corrupts unrelated tests.
#[inline(always)]
pub fn encode_with(base: u64, shift: usize, addr: u64) -> u32 {
    if addr == 0 {
        return 0;
    }
    let delta = addr.wrapping_sub(base);
    // A single unsigned compare covers both `addr < base` (which wraps to a
    // huge delta) and `addr >= limit`. Misalignment is rejected too: the low
    // `shift` bits would be discarded by the shift and decode to a *different*
    // address.
    if delta == 0 || delta > ((u32::MAX as u64) << shift) || (delta & ((1u64 << shift) - 1)) != 0 {
        return report_unencodable(addr);
    }
    (delta >> shift) as u32
}

/// [`decode`] against an EXPLICIT geometry instead of the published globals.
/// See [`encode_with`].
#[inline(always)]
pub fn decode_with(base: u64, shift: usize, narrow: u32) -> u64 {
    if narrow == 0 {
        return 0;
    }
    base + ((narrow as u64) << shift)
}

// --- Reference-slot accessors ------------------------------------------------
//
// Every heap reference slot - a compact instance field and a reference array
// element alike - is read and written through these two functions. They are the
// single chokepoint the compressed-oops wiring depends on: with compression off
// they are byte-for-byte the previous raw 64-bit pointer access, and with it on
// they are a 4-byte narrow load/store plus the base+shift transform.

/// Read a heap reference slot as a raw 64-bit address (`0` = null).
///
/// # Safety
/// `ptr` must point at a live reference slot of the current width
/// ([`ref_field_size`]) inside an object allocated under the same
/// configuration.
#[inline(always)]
pub unsafe fn read_ref_slot(ptr: *const u8) -> u64 {
    if narrow_oops_enabled() {
        decode(unsafe { (ptr as *const u32).read() })
    } else {
        unsafe { (ptr as *const u64).read() }
    }
}

/// Write a raw 64-bit address into a heap reference slot (`0` = null).
///
/// # Safety
/// Same contract as [`read_ref_slot`].
#[inline(always)]
pub unsafe fn write_ref_slot(ptr: *mut u8, addr: u64) {
    probe(addr);
    if narrow_oops_enabled() {
        let n = encode(addr);
        unsafe { (ptr as *mut u32).write(n) }
    } else {
        unsafe { (ptr as *mut u64).write(addr) }
    }
}

/// Unaligned [`read_ref_slot`], for diagnostic walkers that may land on a slot
/// without proving its alignment first.
///
/// # Safety
/// `ptr` must be readable for [`ref_field_size`] bytes.
#[inline]
pub unsafe fn read_ref_slot_unaligned(ptr: *const u8) -> u64 {
    if narrow_oops_enabled() {
        decode(unsafe { (ptr as *const u32).read_unaligned() })
    } else {
        unsafe { (ptr as *const u64).read_unaligned() }
    }
}

// --- Span probe --------------------------------------------------------------
//
// Diagnostic used to derive the heap geometry empirically and to prove after
// the fact that every stored reference stayed inside the encodable window.
// Enabled with `CRATONVM_OOP_SPAN_PROBE=1`; off it costs one relaxed load.

static PROBE_ON: AtomicUsize = AtomicUsize::new(usize::MAX); // MAX = not yet read
static PROBE_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
static PROBE_MAX: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
fn probe_enabled() -> bool {
    let v = PROBE_ON.load(Ordering::Relaxed);
    if v == usize::MAX {
        let on = matches!(
            crate::flags::runtime_var("CRATONVM_OOP_SPAN_PROBE").as_deref(),
            Ok("1")
        ) as usize;
        PROBE_ON.store(on, Ordering::Relaxed);
        return on == 1;
    }
    v == 1
}

/// Record `addr` in the observed reference-address span (no-op unless the
/// probe env var is set).
#[inline(always)]
pub fn probe(addr: u64) {
    if addr == 0 || !probe_enabled() {
        return;
    }
    probe_slow(addr);
}

#[cold]
fn probe_slow(addr: u64) {
    let mut grew = false;
    let mut cur = PROBE_MIN.load(Ordering::Relaxed);
    while addr < cur {
        match PROBE_MIN.compare_exchange_weak(cur, addr, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                grew = true;
                break;
            }
            Err(v) => cur = v,
        }
    }
    let mut cur = PROBE_MAX.load(Ordering::Relaxed);
    while addr > cur {
        match PROBE_MAX.compare_exchange_weak(cur, addr, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                grew = true;
                break;
            }
            Err(v) => cur = v,
        }
    }
    if grew {
        let n = PROBE_REPORTS.fetch_add(1, Ordering::Relaxed);
        if n < 96 {
            let lo = PROBE_MIN.load(Ordering::Relaxed);
            let hi = PROBE_MAX.load(Ordering::Relaxed);
            eprintln!(
                "oop-span-probe[{n}]: min={lo:#x} max={hi:#x} span={:.2} MiB",
                (hi - lo) as f64 / (1024.0 * 1024.0)
            );
        }
    }
}

static PROBE_REPORTS: AtomicUsize = AtomicUsize::new(0);

/// Observed `(min, max)` reference address, or `None` if nothing was recorded.
pub fn probe_span() -> Option<(u64, u64)> {
    let lo = PROBE_MIN.load(Ordering::Relaxed);
    let hi = PROBE_MAX.load(Ordering::Relaxed);
    if lo == u64::MAX {
        None
    } else {
        Some((lo, hi))
    }
}

/// Print the observed span to stderr. Called from VM shutdown when the probe
/// is enabled.
pub fn probe_report() {
    if let Some((lo, hi)) = probe_span() {
        eprintln!(
            "cratonvm oop-span-probe: min={lo:#x} max={hi:#x} span={} bytes ({:.1} MiB) \
             encodable_at_shift3={}",
            hi - lo,
            (hi - lo) as f64 / (1024.0 * 1024.0),
            (hi - lo) <= ((u32::MAX as u64) << 3),
        );
    } else {
        eprintln!("cratonvm oop-span-probe: no references recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: nothing in this module may call `enable` / `disable_for_test`.
    //
    // Those publish the PROCESS-GLOBAL geometry, and a `cargo test` binary runs
    // its tests on parallel threads — so a test that flips the config here is
    // retyping every reference slot in every OTHER test in this binary, including
    // `heap_types::tests::element_byte_size_reference`, which asserts the wide
    // (8-byte) element size. The tests that genuinely need a published geometry
    // live alone in `types/tests/narrow_oop_global_config.rs`; see the warning on
    // [`disable_for_test`] for what this cost in `cratonvm-gc`.
    //
    // Everything below is the pure arithmetic, exercised through `encode_with` /
    // `decode_with`.

    #[test]
    fn roundtrip_shift3() {
        let base = 0x7f00_0000_0000u64;
        for delta in [8u64, 16, 4096, 1 << 20, (u32::MAX as u64) << 3] {
            let addr = base + delta;
            let n = encode_with(base, 3, addr);
            assert_eq!(decode_with(base, 3, n), addr, "delta {delta:#x}");
        }
        assert_eq!(encode_with(base, 3, 0), 0);
        assert_eq!(decode_with(base, 3, 0), 0);
    }

    #[test]
    fn roundtrip_shift0() {
        let base = 0x1000u64;
        for delta in [1u64, 7, 8, 1 << 20, u32::MAX as u64] {
            let addr = base + delta;
            assert_eq!(decode_with(base, 0, encode_with(base, 0, addr)), addr);
        }
    }

    #[test]
    fn the_geometry_is_a_parameter_not_a_global() {
        // The same address encodes differently under two geometries, and each
        // decodes back through its own — the property that lets a test exercise
        // the codec without publishing anything.
        let addr = 0x7f00_0000_1000u64;
        let a = encode_with(0x7f00_0000_0000, 3, addr);
        let b = encode_with(0x7f00_0000_0800, 3, addr);
        assert_ne!(a, b);
        assert_eq!(decode_with(0x7f00_0000_0000, 3, a), addr);
        assert_eq!(decode_with(0x7f00_0000_0800, 3, b), addr);
        assert!(
            !narrow_oops_enabled(),
            "a unit test must not leave the process-global geometry published"
        );
    }
}
