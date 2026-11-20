// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Call-site memoization for native-method dispatch.
//!
//! # Why this module exists
//!
//! `NativeMethodRegistry::find` resolves a native by FNV-1a-hashing all three
//! of `class_name`, `method_name` and `descriptor` on **every call** (see
//! `registry::native_method_hash`). At ~3,100 registrations the map probe
//! itself is O(1), but the hash is not free: it is a byte-at-a-time walk over
//! three strings whose combined length is routinely 60-150 bytes (JVM
//! descriptors are long). Two independent profiling sessions caught this:
//!
//!  * `perf` on `TestResponsePerformance` put `NativeMethodRegistry::find` at
//!    ~7% of all samples — the #2 hottest symbol behind the interpreter's own
//!    frame-dispatch loop (see `jit_api::CachedBytecodeMethod::native_callback_cache`).
//!  * a `gdb` sampling profile of an H2 `TestFileSystem.testConcurrent` run
//!    caught both live threads inside `hash_byte_pair` / `native_method_hash`
//!    disproportionately often (see `NativeMethodRegistry::find_with_kind`).
//!
//! Both were fixed *locally*, at one call site each, with bespoke mechanisms.
//! This module replaces those one-offs with a single shared mechanism:
//!
//!  * [`NativeMethodId`] — a dense `u32` index into the registry's slot table.
//!    Resolving one costs the same as `find`; *using* one costs a bounds-checked
//!    array index. A call site resolves once and stores the id.
//!  * [`NativeMethodKey`] — the 128-bit digest of a triple, precomputable at
//!    class-link time so a name-based lookup never re-hashes constant strings.
//!  * [`NativeCallSite`] — a one-word, `Sync`, self-invalidating memo cell that
//!    a caller embeds next to its cached method metadata.
//!
//! # Soundness: the digest is never trusted on its own
//!
//! `classloading/src/class_manager.rs` (see the `loaded_classes` field doc,
//! "Round 4 audit fix (CRIT)") documents a real defect of exactly this shape: a
//! `name_to_id: FxHashMap<u64, ClassId>` shadow map keyed by a raw FNV-1a digest
//! **with no name verification**, so any collision returned the wrong `ClassId`
//! and produced silent type confusion downstream. That map was deleted.
//!
//! Every digest-keyed lookup added here therefore re-checks the full
//! `(class, method, descriptor)` triple against the registration that owns the
//! slot before returning it. A digest hit whose name does not match is reported
//! as a **miss**, never as a wrong-but-plausible callback. See
//! `NativeMethodRegistry::slot_index_for_key`.
//!
//! # Handle stability
//!
//! A [`NativeMethodId`] is stable for the life of the registry: re-registering
//! the same triple (which `alias_class` and several `register_*` passes do)
//! updates the existing slot **in place** rather than appending a new one, so a
//! handle handed out before the re-registration keeps resolving — and resolves
//! to the *new* callback, matching the last-registration-wins contract of
//! `register`. Ids are dense and monotonically assigned; they are never reused
//! or invalidated, because the registry has no removal path.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::registry::{NativeCallback, NativeKind, NativeMethodRegistry};

/// A stable, dense handle to one registered native method.
///
/// Obtain one from [`NativeMethodRegistry::resolve_id`] (or from a
/// [`NativeCallSite`]); redeem it with [`NativeMethodRegistry::callback_of`],
/// which is a bounds-checked array index — no hashing, no string walk.
///
/// Handles are only meaningful against the registry that issued them. In a
/// running VM there is exactly one (`shared.natives.native_methods`), built during boot
/// and never replaced; tests that build their own `NativeMethodRegistry` must
/// not mix handles between instances. Redeeming a foreign handle is *safe* (it
/// either bounds-checks to `None` or returns some other registered native), but
/// it is a caller bug — this is why [`NativeCallSite`] never exposes a way to
/// inject a raw id.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NativeMethodId(u32);

impl NativeMethodId {
    /// Wrap a raw slot index. Only for callers round-tripping an id they
    /// previously obtained from [`NativeMethodId::as_u32`] on the *same*
    /// registry (e.g. packing into an atomic word).
    #[inline]
    pub const fn from_u32(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw slot index, for packing into a compact cache word.
    #[inline]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// The slot index as a `usize`.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Precomputed 128-bit digest of a `(class, method, descriptor)` triple.
///
/// The point of exposing this is item 3 of the memoization work: a call site
/// that *must* resolve by name (because it needs the strings anyway, e.g. for
/// the full-name verification) should not re-hash constant strings that were
/// already known at class-link time. Build one alongside the method metadata
/// and hand it to [`NativeMethodRegistry::resolve_id_by_key`] /
/// [`NativeMethodRegistry::find_by_key`].
///
/// **This is a hash, not an identity.** It is deliberately *not* usable as a
/// lookup key on its own — every `*_by_key` entry point still takes the three
/// strings and verifies them on a hit. See the module docs for the
/// `class_manager` history that makes this non-negotiable.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct NativeMethodKey {
    pub(crate) lo: u64,
    pub(crate) hi: u64,
}

impl NativeMethodKey {
    /// Hash a triple. Identical to the digest `NativeMethodRegistry::find`
    /// computes internally, so a key built here and a name-based lookup agree.
    #[inline]
    pub fn new(class_name: &str, method_name: &str, descriptor: &str) -> Self {
        let (lo, hi) = crate::registry::native_method_hash(class_name, method_name, descriptor);
        Self { lo, hi }
    }

    /// The raw `(u64, u64)` pair, for callers that want to store it unpacked.
    #[inline]
    pub const fn as_pair(self) -> (u64, u64) {
        (self.lo, self.hi)
    }
}

/// Bit layout of [`NativeCallSite::memo`].
///
/// ```text
///  63                    32 31                     0
/// +------------------------+------------------------+
/// | registry generation    | slot + 1  (0 = "none") |
/// +------------------------+------------------------+
/// ```
///
/// The all-zero word means "never resolved", unambiguously:
/// `NativeMethodRegistry::generation()` is banded per registry and is never
/// `0`, so no filled memo can encode to zero.
///
/// The generation half also identifies *which* registry the memo was taken
/// against, so a cell that outlives one registry (a `static` in a test binary
/// that builds several VMs) re-resolves rather than redeeming a slot index that
/// means something else in the other registry.
pub use cratonvm_types::native_call_site::{NativeCallSite, MEMO_EMPTY};

/// Resolution and memoization operations on [`NativeCallSite`].
pub trait NativeCallSiteExt {
    fn resolve(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId>;
    fn resolve_with_key(
        &self,
        registry: &NativeMethodRegistry,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId>;
    fn callback(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback>;
    fn callback_with_kind(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<(NativeCallback, NativeKind)>;
    fn is_warm(&self, registry: &NativeMethodRegistry) -> bool;
}

#[inline]
#[cfg(debug_assertions)]
fn witness_triple(site: &NativeCallSite, class_name: &str, method_name: &str, descriptor: &str) -> bool {
    let (lo, hi) = crate::registry::native_method_hash(class_name, method_name, descriptor);
    let digest = (lo ^ hi.rotate_left(32)) | 1;
    let prev = site.triple_witness.swap(digest, Ordering::Relaxed);
    prev == 0 || prev == digest
}

#[inline]
#[cfg(not(debug_assertions))]
#[allow(dead_code)]
fn witness_triple(_site: &NativeCallSite, _class_name: &str, _method_name: &str, _descriptor: &str) -> bool {
    true
}

#[inline]
fn check_one_triple_per_site(site: &NativeCallSite, class_name: &str, method_name: &str, descriptor: &str) {
    debug_assert!(
        witness_triple(site, class_name, method_name, descriptor),
        "NativeCallSite reused for a second (class, method, descriptor) triple (now {}.{}{}).",
        class_name,
        method_name,
        descriptor
    );
}

#[inline]
fn decode_memo(memo: u64) -> Option<NativeMethodId> {
    let encoded = memo as u32;
    if encoded == 0 {
        None
    } else {
        Some(NativeMethodId::from_u32(encoded - 1))
    }
}

#[inline]
fn store_memo(site: &NativeCallSite, generation: u32, id: Option<NativeMethodId>) {
    let encoded = match id {
        Some(id) => id.as_u32().saturating_add(1),
        None => 0,
    };
    site.memo.store(((generation as u64) << 32) | encoded as u64, Ordering::Relaxed);
}

#[cold]
#[inline(never)]
fn fill_memo(
    site: &NativeCallSite,
    registry: &NativeMethodRegistry,
    generation: u32,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<NativeMethodId> {
    let id = registry.resolve_id(class_name, method_name, descriptor);
    store_memo(site, generation, id);
    id
}

impl NativeCallSiteExt for NativeCallSite {
    #[inline]
    fn resolve(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        check_one_triple_per_site(self, class_name, method_name, descriptor);
        let generation = registry.generation();
        let memo = self.memo.load(Ordering::Relaxed);
        if memo != MEMO_EMPTY && (memo >> 32) as u32 == generation {
            return decode_memo(memo);
        }
        fill_memo(self, registry, generation, class_name, method_name, descriptor)
    }

    #[inline]
    fn resolve_with_key(
        &self,
        registry: &NativeMethodRegistry,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        check_one_triple_per_site(self, class_name, method_name, descriptor);
        let generation = registry.generation();
        let memo = self.memo.load(Ordering::Relaxed);
        if memo != MEMO_EMPTY && (memo >> 32) as u32 == generation {
            return decode_memo(memo);
        }
        let id = registry.resolve_id_by_key(key, class_name, method_name, descriptor);
        store_memo(self, generation, id);
        id
    }

    #[inline]
    fn callback(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        let id = self.resolve(registry, class_name, method_name, descriptor)?;
        registry.callback_of(id)
    }

    #[inline]
    fn callback_with_kind(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<(NativeCallback, NativeKind)> {
        let id = self.resolve(registry, class_name, method_name, descriptor)?;
        Some((registry.callback_of(id)?, registry.kind_of_id(id)?))
    }

    #[inline]
    fn is_warm(&self, registry: &NativeMethodRegistry) -> bool {
        let memo = self.memo.load(Ordering::Relaxed);
        memo != MEMO_EMPTY && (memo >> 32) as u32 == registry.generation()
    }
}

#[cfg(test)]
mod tests;
