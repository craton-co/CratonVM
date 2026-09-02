// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The per-object extra-root filter: four global lookups per marked object,
//! reduced to two indexed loads.
//!
//! # What this is for
//!
//! Every marked object's out-edges are its own reference slots PLUS four
//! things the heap does not store in it:
//!
//! | edge | keyed by | table |
//! |---|---|---|
//! | the pinned defining `ClassLoader` | CLASS id | `loader_pin` |
//! | the class mirrors a loader owns | owner ADDRESS | `mirror_pin` |
//! | the metadata a loader owns | owner ADDRESS | `metadata_pin` |
//! | a native collection's overlay | owner ADDRESS | `external_roots` |
//!
//! Each of the three mark loops asks all four, per object, and each answer
//! costs an `RwLock` read plus a hash lookup that overwhelmingly returns
//! nothing. Ten million marked objects is forty million lock-and-hash
//! operations to learn four times over that an ordinary `String` owns no
//! classes.
//!
//! # Why the existing fast paths are not enough
//!
//! Three of the four latch a `NON_EMPTY` flag and exit on one relaxed load
//! when their table has never been written -- so on a program that loads no
//! class through a user `ClassLoader` they really are free, and this filter
//! changes nothing.
//!
//! `external_roots` is the exception, and it is the one that matters:
//! `native-collections` registers its overlay provider at VM startup, so
//! `PROVIDER_COUNT` is non-zero on *every* run and the latch never fires. Each
//! marked object takes the provider `RwLock`, enters the callback, and takes
//! that module's own `Mutex` over the owner index. That is the per-object cost
//! this filter exists to remove, and the other three come along for free
//! because they are keyed the same way.
//!
//! # The shape
//!
//! Two lock-free structures, built once per collection and read per object:
//!
//! * a **class-id bitmap** for the loader pin -- exact, because class ids are
//!   dense and small, with an overflow latch for the ones that do not fit;
//! * an **address bloom** for the other three, which share a key space. A
//!   false positive costs one real lookup that returns nothing, which is the
//!   status quo; a false negative would drop a live edge, and the bloom cannot
//!   produce one because every owner address is inserted before the mark
//!   begins and no owner is added during it (the tables are written by class
//!   definition and by native registration, both of which are mutator work).
//!
//! Same two-hash bloom the reference-skip set uses a few hundred lines away in
//! `zgc.rs`, for the same reason: it is the cheapest structure that can answer
//! "definitely not" without a lock.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 512 words = 32 Kib of bloom, over however many owner addresses exist.
/// Owners are class loaders and native collection handles -- thousands at
/// most, against 32,768 bits, so the false-positive rate stays negligible
/// without tuning.
pub(crate) const Z_ROOT_BLOOM_WORDS: usize = 512;

/// 1,024 words = 65,536 class ids, exactly. Beyond that the overflow latch
/// sends every lookup down the slow path, which is correct and merely slower.
pub(crate) const Z_ROOT_CLASS_WORDS: usize = 1024;

/// `CRATONVM_ZGC_MARK_ROOT_FILTER`: consult the filter before the four global
/// tables. Default on; `0`/`off`/`false`/`no` asks them per object as before.
///
/// Read per COLLECTION, not latched, so both arms can run in one process --
/// which is what `the_filter_and_the_tables_agree_about_every_object` needs.
pub(crate) fn filter_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_MARK_ROOT_FILTER") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

/// Which objects and classes have edges the heap does not store in them.
///
/// Rebuilt at the start of every mark. Reading it never takes a lock, which is
/// the whole point: it is consulted once per marked object.
pub(crate) struct ZMarkRootFilter {
    /// Class ids with a pinned defining loader.
    class_bits: Box<[AtomicU64; Z_ROOT_CLASS_WORDS]>,
    /// Owner addresses in `mirror_pin`, `metadata_pin` or any external-root
    /// provider.
    addr_bloom: Box<[AtomicU64; Z_ROOT_BLOOM_WORDS]>,
    /// A class id too large for `class_bits`: every class then takes the slow
    /// path, because the bitmap can no longer answer "definitely not".
    class_overflow: AtomicBool,
    /// Is any class pinned at all? Skips even the bitmap load.
    any_class: AtomicBool,
    /// Is any address an owner at all?
    any_addr: AtomicBool,
    /// At least one external-root provider declined to enumerate its owners,
    /// so no address may be excluded on its behalf. See
    /// `ExternalRootProvider::owner_addrs`.
    overlay_unfilterable: AtomicBool,
    /// Was this filter ever built? An unbuilt filter answers "maybe" to
    /// everything, so a caller that forgets to arm it is slow, not wrong.
    armed: AtomicBool,
}

impl Default for ZMarkRootFilter {
    fn default() -> Self {
        Self {
            class_bits: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
            addr_bloom: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
            class_overflow: AtomicBool::new(false),
            any_class: AtomicBool::new(false),
            any_addr: AtomicBool::new(false),
            overlay_unfilterable: AtomicBool::new(false),
            armed: AtomicBool::new(false),
        }
    }
}

impl std::fmt::Debug for ZMarkRootFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZMarkRootFilter")
            .field("armed", &self.armed.load(Ordering::Relaxed))
            .field("any_class", &self.any_class.load(Ordering::Relaxed))
            .field("any_addr", &self.any_addr.load(Ordering::Relaxed))
            .field("class_overflow", &self.class_overflow.load(Ordering::Relaxed))
            .finish()
    }
}

impl ZMarkRootFilter {
    /// Snapshot the four tables' KEY SETS. Called once, at mark start, with
    /// the world stopped or the tables otherwise quiescent.
    ///
    /// `CRATONVM_ZGC_MARK_ROOT_FILTER=0` leaves it disarmed, which restores
    /// the per-object lookups exactly -- the arms differ only in whether a
    /// question is asked of a bloom or of four global tables, so the switch is
    /// a clean A/B rather than a different collector.
    pub(crate) fn rebuild(&self) {
        if !filter_enabled() {
            self.disarm();
            return;
        }
        for w in self.class_bits.iter() {
            w.store(0, Ordering::Relaxed);
        }
        for w in self.addr_bloom.iter() {
            w.store(0, Ordering::Relaxed);
        }
        self.class_overflow.store(false, Ordering::Relaxed);

        let mut any_class = false;
        for class_id in cratonvm_types::loader_pin::pinned_class_ids() {
            any_class = true;
            let bit = class_id as usize;
            match self.class_bits.get(bit >> 6) {
                Some(word) => {
                    word.fetch_or(1u64 << (bit & 63), Ordering::Relaxed);
                }
                None => self.class_overflow.store(true, Ordering::Relaxed),
            }
        }
        self.any_class.store(any_class, Ordering::Relaxed);

        let mut any_addr = false;
        let mut insert = |addr: usize| {
            any_addr = true;
            let (w1, b1, w2, b2) = Self::slots(addr);
            self.addr_bloom[w1].fetch_or(1u64 << b1, Ordering::Relaxed);
            self.addr_bloom[w2].fetch_or(1u64 << b2, Ordering::Relaxed);
        };
        for addr in cratonvm_types::mirror_pin::pinned_owner_addrs() {
            insert(addr);
        }
        if let Some(metadata) = cratonvm_types::metadata_pin::snapshot() {
            for owner in metadata.keys() {
                insert(*owner);
            }
        }
        let (overlay_owners, overlay_complete) =
            crate::external_roots::owner_addrs_and_completeness();
        for owner in overlay_owners {
            insert(owner);
        }
        self.overlay_unfilterable
            .store(!overlay_complete, Ordering::Relaxed);
        self.any_addr.store(any_addr, Ordering::Relaxed);
        self.armed.store(true, Ordering::Release);
    }

    /// Disarm. Every question then answers "maybe", which is the behaviour
    /// that existed before this filter did.
    pub(crate) fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
    }

    /// Could the class at `class_id` have a pinned defining loader?
    ///
    /// `false` is a proof; `true` means ask the real table.
    #[inline]
    pub(crate) fn may_pin_loader(&self, class_id: u32) -> bool {
        if !self.armed.load(Ordering::Acquire) {
            return true;
        }
        if !self.any_class.load(Ordering::Relaxed) {
            return false;
        }
        let bit = class_id as usize;
        match self.class_bits.get(bit >> 6) {
            Some(word) => word.load(Ordering::Relaxed) & (1u64 << (bit & 63)) != 0,
            None => self.class_overflow.load(Ordering::Relaxed),
        }
    }

    /// Could the object at `addr` own mirrors, metadata or a native overlay?
    ///
    /// `false` is a proof; `true` means ask the three real tables.
    #[inline]
    pub(crate) fn may_own_extra_roots(&self, addr: usize) -> bool {
        if !self.armed.load(Ordering::Acquire) {
            return true;
        }
        if !self.any_addr.load(Ordering::Relaxed) {
            return false;
        }
        let (w1, b1, w2, b2) = Self::slots(addr);
        self.addr_bloom[w1].load(Ordering::Relaxed) & (1u64 << b1) != 0
            && self.addr_bloom[w2].load(Ordering::Relaxed) & (1u64 << b2) != 0
    }

    /// Could the object at `addr` own a native-collection overlay?
    ///
    /// Separate from [`Self::may_own_extra_roots`] because the overlay
    /// providers are the one source that can decline to enumerate its owners.
    /// When any of them does, this answers `true` for everything and the
    /// per-object lookup runs exactly as it did before the filter existed --
    /// a provider that will not say which objects own roots may own any of
    /// them, and excluding one on its behalf would drop a live edge.
    #[inline]
    pub(crate) fn may_own_overlay(&self, addr: usize) -> bool {
        if self.overlay_unfilterable.load(Ordering::Relaxed) {
            return true;
        }
        self.may_own_extra_roots(addr)
    }

    /// Two independent positions from one splitmix64 pass over the address's
    /// 8-byte grid index. Identical construction to `ZgcRealHeap::skip_bloom_slots`.
    #[inline]
    fn slots(addr: usize) -> (usize, u32, usize, u32) {
        let mut x = (addr >> 3) as u64;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        x ^= x >> 33;
        let bits = (Z_ROOT_BLOOM_WORDS * 64) as u64;
        let h1 = x % bits;
        let h2 = (x >> 32) % bits;
        (
            (h1 / 64) as usize,
            (h1 % 64) as u32,
            (h2 / 64) as usize,
            (h2 % 64) as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The filter must never answer "definitely not" for something that IS
    /// registered -- a false negative drops a live edge and the object it
    /// names is swept while reachable.
    #[test]
    fn a_registered_owner_is_never_rejected() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        // Whatever this process's tables hold, every address they hold must
        // pass. Built from the same accessors the rebuild reads, so this also
        // catches an accessor that stops reporting a table.
        for addr in cratonvm_types::mirror_pin::pinned_owner_addrs() {
            assert!(filter.may_own_extra_roots(addr), "mirror owner {addr:#x} rejected");
        }
        if let Some(metadata) = cratonvm_types::metadata_pin::snapshot() {
            for owner in metadata.keys() {
                assert!(
                    filter.may_own_extra_roots(*owner),
                    "metadata owner {owner:#x} rejected"
                );
            }
        }
        if let Some(owners) = crate::external_roots::external_owner_addrs() {
            for owner in owners {
                assert!(
                    filter.may_own_extra_roots(owner),
                    "overlay owner {owner:#x} rejected"
                );
            }
        }
        for class_id in cratonvm_types::loader_pin::pinned_class_ids() {
            assert!(filter.may_pin_loader(class_id), "class {class_id} rejected");
        }
    }

    /// A provider that will not enumerate its owners must disable overlay
    /// filtering ENTIRELY, for every address.
    ///
    /// This is the soundness case, and it is not hypothetical: `zgc.rs`'s own
    /// test provider returns `None` from `owner_addrs` while serving roots
    /// from `roots_for_owner`. An address bloom built from an index that
    /// provider is absent from would answer "definitely not" for the very
    /// object it owns roots for, and the overlay would be swept while its
    /// owner survived.
    #[test]
    fn a_provider_that_will_not_name_its_owners_disables_the_overlay_filter() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        // Simulate the decline rather than registering a provider: provider
        // registration is process-global and permanent, so a test that did it
        // for real would change every later test in this binary.
        filter.overlay_unfilterable.store(true, Ordering::Relaxed);
        for addr in [0x1000usize, 0x2000, 0xdead_beef_0000] {
            assert!(
                filter.may_own_overlay(addr),
                "{addr:#x} was excluded from the overlay lookup while a provider \
                 had declined to enumerate its owners"
            );
        }
        // The other two tables are unaffected: their key sets are exact.
        filter.overlay_unfilterable.store(false, Ordering::Relaxed);
        if !filter.any_addr.load(Ordering::Relaxed) {
            assert!(!filter.may_own_overlay(0x1000));
        }
    }

    /// The switch must leave the filter DISARMED, not merely empty. An empty
    /// armed filter answers "definitely not" to everything, which is the
    /// opposite of the fallback it is supposed to restore.
    #[test]
    fn the_kill_switch_disarms_rather_than_emptying() {
        let filter = ZMarkRootFilter::default();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_MARK_ROOT_FILTER", Some("0"))],
            || {
                assert!(!filter_enabled());
                filter.rebuild();
            },
        );
        assert!(
            filter.may_own_extra_roots(0x4000) && filter.may_pin_loader(3),
            "the switched-off filter excluded something; it must prove nothing"
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_MARK_ROOT_FILTER", Some("1"))],
            || {
                assert!(filter_enabled());
                filter.rebuild();
            },
        );
    }

    /// An unbuilt filter must answer "maybe" to everything: a caller that
    /// forgets to arm it gets the old cost, not a dropped edge.
    #[test]
    fn an_unarmed_filter_proves_nothing() {
        let filter = ZMarkRootFilter::default();
        assert!(filter.may_own_extra_roots(0xdead_0000));
        assert!(filter.may_pin_loader(7));
        assert!(filter.may_pin_loader(u32::MAX));
        filter.rebuild();
        filter.disarm();
        assert!(filter.may_own_extra_roots(0xdead_0000));
        assert!(filter.may_pin_loader(7));
    }

    /// A class id past the bitmap must fall back rather than answer from a
    /// bit it does not have.
    #[test]
    fn a_class_id_past_the_bitmap_takes_the_slow_path() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        filter.any_class.store(true, Ordering::Relaxed);
        let past = (Z_ROOT_CLASS_WORDS * 64) as u32;
        // No overflow recorded: nothing that large is registered, so the
        // bitmap's "definitely not" still stands.
        assert!(!filter.may_pin_loader(past));
        filter.class_overflow.store(true, Ordering::Relaxed);
        assert!(
            filter.may_pin_loader(past),
            "with a class id past the bitmap registered, every out-of-range id \
             must take the real lookup"
        );
    }

    /// The empty case is the common one and must cost nothing: with no table
    /// written, every question is answered by one flag.
    #[test]
    fn an_empty_process_rejects_every_address() {
        let filter = ZMarkRootFilter::default();
        filter.rebuild();
        if filter.any_addr.load(Ordering::Relaxed) {
            return; // another test in this process registered something
        }
        for addr in [0x1000usize, 0x2000, 0xffff_0000] {
            assert!(!filter.may_own_extra_roots(addr));
        }
    }
}
