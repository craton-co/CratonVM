// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Uniform GC-root registry for process-global native side-tables.
//!
//! # Why this exists
//!
//! A number of native subsystems hold `ObjectRef`s in **process-global Rust
//! side-tables** that are invisible to the field-tracing root scan in
//! [`crate::memory::roots::collect_roots`]. Historically each such subsystem
//! had to be wired in by hand in TWO places:
//!
//!   * a `gc_scan_*_roots(&mut Vec<ObjectRef>)` call appended to `collect_roots`
//!     (so the held objects are marked live), and
//!   * a `gc_update_*_refs(&HashMap<usize, usize>)` call appended to
//!     `gc::update_all_roots` (so the held `ObjectRef`s are repointed to their
//!     relocated addresses after a moving/compacting collection).
//!
//! Forgetting **either** half is a use-after-free: a missing scan lets a moving
//! young-gen GC reclaim an object still referenced by the side-table; a missing
//! remap leaves the side-table pointing at a vacated from-space slot. The review
//! found ~8 subsystems in exactly this position (native-collection overlays, the
//! NIO `SelectionKey` table, the `ClassFileTransformer` chain, the
//! `ObjectStreamClass` cache, value-stack-smuggled jobjects, the scheduled pump,
//! and the xnio `IoFuture` table).
//!
//! # What this provides
//!
//! A single global registry. A subsystem calls
//! [`register_native_root_source`] **once** (typically from its lazy
//! initialization), supplying two top-level function pointers:
//!
//!   * a **scan** callback — `fn(&mut Vec<ObjectRef>)` — that pushes each live
//!     `ObjectRef` it currently holds (identical shape to the existing
//!     `gc_scan_*_roots` helpers, so a subsystem can register its existing
//!     function verbatim), and
//!   * a **remap** callback — `fn(&HashMap<usize, usize>)` — that rewrites each
//!     held `ObjectRef` using the collector's old→new relocation map (identical
//!     shape to the existing `gc_update_*_refs` helpers).
//!
//! [`scan_all_native_roots`] (driven from `collect_roots`) fans out to every
//! registered scan callback; [`remap_all_native_roots`] (driven from the
//! post-move fixup in `gc::update_all_roots`) fans out to every registered remap
//! callback. With an empty registry both are no-ops, so this is a **zero
//! behaviour-change** addition that is safe to merge before any subsystem
//! adopts it.
//!
//! # Thread / lifecycle safety
//!
//! Registration is safe from any thread, before or after the GC has started —
//! the registry is a `LazyLock<RwLock<…>>` (mirroring the JNI-native-method
//! table in `native::jni`). Registration is **idempotent**: re-registering the
//! same `(scan, remap)` function-pointer pair is silently ignored, so a
//! subsystem that registers lazily on first use cannot double-scan (which would
//! merely over-retain) or double-remap.
//!
//! The callbacks run while the world is stopped (root collection / post-move
//! fixup), exactly like every other entry in `collect_roots` /
//! `update_all_roots`, so they observe a quiescent heap.

use crate::types::ObjectRef;
use std::collections::HashMap;
use std::sync::LazyLock;

use parking_lot::RwLock;

/// A scan callback: append every live `ObjectRef` the subsystem currently holds
/// to `roots`. Shape-compatible with the existing `gc_scan_*_roots` helpers so
/// a subsystem can register one of those directly.
pub type ScanFn = fn(&mut Vec<ObjectRef>);

/// A remap callback: for each `ObjectRef` the subsystem holds, look its current
/// address up in `pointer_map` (the collector's old→new relocation map) and, if
/// present, rewrite it to the new address. Shape-compatible with the existing
/// `gc_update_*_refs` helpers. A well-behaved implementation early-returns on an
/// empty map (nothing moved — the non-moving sweep).
pub type RemapFn = fn(&HashMap<usize, usize>);

/// One registered native root source: a paired scan + remap callback. The two
/// halves describe the same set of held `ObjectRef`s — scanning keeps them live,
/// remapping keeps them valid across a move.
#[derive(Clone, Copy)]
struct NativeRootSource {
    scan: ScanFn,
    remap: RemapFn,
}

/// Process-global registry of native root sources.
///
/// Modelled on `native::jni::JNI_NATIVE_METHODS` (`LazyLock<RwLock<…>>`): cheap
/// to read (the GC pause path takes a read lock and iterates), rare to write
/// (subsystems register once at init). `parking_lot::RwLock` matches the rest of
/// the VM and is poison-free, so a panicking callback cannot brick the registry.
static REGISTRY: LazyLock<RwLock<Vec<NativeRootSource>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a native root source: a `scan` callback that yields every live
/// `ObjectRef` the subsystem holds, and a `remap` callback that repoints each
/// held `ObjectRef` after a moving collection.
///
/// Call this **once** per subsystem, typically from its lazy initialization.
/// Registration is idempotent: registering the same `(scan, remap)` pair more
/// than once is a no-op, so a subsystem that initializes lazily (and might race
/// to register from multiple threads) never ends up double-scanned.
///
/// Safe to call from any thread, before or after GC has started. The callbacks
/// are invoked only at a GC safepoint (root collection / post-move fixup) with
/// the world stopped, so they observe a quiescent heap — exactly the contract
/// the existing hand-wired `gc_scan_*` / `gc_update_*` helpers rely on.
///
/// # Function-pointer requirement
///
/// `scan` and `remap` are `fn` pointers (not `Box<dyn Fn>`): every subsystem
/// registers a top-level `fn`, which sidesteps lifetime/`'static`-closure
/// concerns and keeps the registry `Copy`-cheap to iterate. The matched pair is
/// deduplicated by comparing the raw code addresses of the two pointers.
pub fn register_native_root_source(scan: ScanFn, remap: RemapFn) {
    let mut reg = REGISTRY.write();
    // Idempotent: dedupe by the raw code addresses of BOTH pointers. Two
    // distinct `fn` items have distinct addresses; the same `fn` item compares
    // equal, so a subsystem that registers lazily on first use (possibly racing
    // across threads) registers exactly once.
    let scan_addr = scan as usize;
    let remap_addr = remap as usize;
    let already = reg
        .iter()
        .any(|s| s.scan as usize == scan_addr && s.remap as usize == remap_addr);
    if !already {
        reg.push(NativeRootSource { scan, remap });
    }
}

/// Run every registered scan callback, appending each subsystem's live
/// `ObjectRef`s to `roots`. Called from
/// [`crate::memory::roots::collect_roots`] alongside the hand-wired root
/// sources. No-op (zero behaviour change) when the registry is empty.
///
/// A read lock is held for the duration so registration cannot race the fan-out;
/// since callbacks only *read* their side-tables and *push* into `roots`, this
/// can never deadlock against another `scan_all` / `remap_all`.
pub fn scan_all_native_roots(roots: &mut Vec<ObjectRef>) {
    let reg = REGISTRY.read();
    for source in reg.iter() {
        (source.scan)(roots);
    }
}

/// Run every registered remap callback, repointing each subsystem's held
/// `ObjectRef`s through `pointer_map`. Called from
/// [`crate::memory::gc::update_all_roots`] alongside the hand-wired
/// `*_update_after_gc` hooks, on the post-move fixup path. No-op when the
/// registry is empty.
///
/// `pointer_map` is the collector's old-address → new-address relocation map (an
/// empty map means nothing moved — the non-moving sweep); each callback is
/// expected to honour that and early-return.
pub fn remap_all_native_roots(pointer_map: &HashMap<usize, usize>) {
    let reg = REGISTRY.read();
    for source in reg.iter() {
        (source.remap)(pointer_map);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // The registry is process-global, so these tests register their OWN unique
    // top-level callbacks (each test uses a distinct `fn` so the dedupe key is
    // unique) and only assert on the refs THEY contribute — never on the total
    // count, which other registrations (real or test) may inflate. All test
    // refs use synthetic, 8-byte-aligned non-heap addresses; they are never
    // dereferenced, only compared by pointer value.

    /// Fabricate a never-dereferenced `ObjectRef` from a fixed aligned address.
    fn fake_ref(addr: usize) -> ObjectRef {
        debug_assert!(addr != 0 && addr % 8 == 0);
        // SAFETY: the address is non-null and 8-byte aligned; the ref is only
        // ever compared by pointer value in these tests, never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    // ----- source #1: a fixed pair of refs, remapped via the pointer map -----
    const REF_A: usize = 0xAAAA_0000;
    const REF_B: usize = 0xBBBB_0000;

    fn scan_src1(roots: &mut Vec<ObjectRef>) {
        roots.push(fake_ref(REF_A));
        roots.push(fake_ref(REF_B));
    }
    fn remap_src1(pointer_map: &HashMap<usize, usize>) {
        // Record into a side-channel that the remap ran and with what mapping,
        // so the test can verify the relocation function was applied.
        if let Some(&new_a) = pointer_map.get(&REF_A) {
            SRC1_REMAP_A.store(new_a, Ordering::SeqCst);
        }
    }
    static SRC1_REMAP_A: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn scan_all_visits_registered_refs() {
        register_native_root_source(scan_src1, remap_src1);

        let mut roots = Vec::new();
        scan_all_native_roots(&mut roots);

        assert!(
            roots.contains(&fake_ref(REF_A)),
            "scan_all must surface the source's first ref"
        );
        assert!(
            roots.contains(&fake_ref(REF_B)),
            "scan_all must surface the source's second ref"
        );
    }

    #[test]
    fn remap_all_applies_relocation() {
        register_native_root_source(scan_src1, remap_src1);

        let mut pointer_map = HashMap::new();
        let relocated_a = 0xCCCC_0000usize;
        pointer_map.insert(REF_A, relocated_a);

        SRC1_REMAP_A.store(0, Ordering::SeqCst);
        remap_all_native_roots(&pointer_map);

        assert_eq!(
            SRC1_REMAP_A.load(Ordering::SeqCst),
            relocated_a,
            "remap_all must invoke the source's remap with the relocation map"
        );
    }

    // ----- source #2: counts how many times its scan callback fired ----------
    static SRC2_SCANS: AtomicUsize = AtomicUsize::new(0);

    fn scan_src2(_roots: &mut Vec<ObjectRef>) {
        SRC2_SCANS.fetch_add(1, Ordering::SeqCst);
    }
    fn remap_src2(_pointer_map: &HashMap<usize, usize>) {}

    #[test]
    fn registration_is_idempotent() {
        // Register the SAME pair three times; the dedupe must keep exactly one.
        register_native_root_source(scan_src2, remap_src2);
        register_native_root_source(scan_src2, remap_src2);
        register_native_root_source(scan_src2, remap_src2);

        SRC2_SCANS.store(0, Ordering::SeqCst);
        let mut roots = Vec::new();
        scan_all_native_roots(&mut roots);

        assert_eq!(
            SRC2_SCANS.load(Ordering::SeqCst),
            1,
            "an idempotently-registered source must scan exactly once per fan-out"
        );
    }

    // ----- source #3: proves an empty pointer-map is handled gracefully ------
    fn scan_src3(_roots: &mut Vec<ObjectRef>) {}
    fn remap_src3(pointer_map: &HashMap<usize, usize>) {
        // A well-behaved remap early-returns on the non-moving sweep.
        if pointer_map.is_empty() {
            SRC3_SAW_EMPTY.store(true, Ordering::SeqCst);
        }
    }
    static SRC3_SAW_EMPTY: AtomicBool = AtomicBool::new(false);

    #[test]
    fn remap_all_with_empty_map_is_safe() {
        register_native_root_source(scan_src3, remap_src3);

        SRC3_SAW_EMPTY.store(false, Ordering::SeqCst);
        let empty: HashMap<usize, usize> = HashMap::new();
        remap_all_native_roots(&empty);

        assert!(
            SRC3_SAW_EMPTY.load(Ordering::SeqCst),
            "remap_all must still fan out (callbacks self-guard the empty map)"
        );
    }
}
