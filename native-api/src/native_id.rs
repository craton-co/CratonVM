// SPDX-License-Identifier: Apache-2.0
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
const MEMO_EMPTY: u64 = 0;

/// A one-word memo of "which native, if any, does this call site dispatch to".
///
/// Embed one next to a call site's cached method metadata (the intended home is
/// `jit_api::CachedBytecodeMethod`, alongside `force_native_cache`), then call
/// [`NativeCallSite::callback`] instead of `NativeMethodRegistry::find`. The
/// first call resolves and stores; every later call is an atomic load, a
/// generation compare, and an array index.
///
/// # Why a generation, when the existing cache is a `OnceLock`
///
/// `CachedBytecodeMethod::native_callback_cache` is a
/// `OnceLock<Option<NativeCallback>>` whose soundness argument is "native
/// registration is immutable after VM boot". That is true of the *steady state*
/// but not of boot itself, and it is not true of `alias_class` or of the lazy
/// `register_*` passes that run after the first bytecode executes: a `None`
/// memoized before those run is wrong forever. Keying the memo on the registry
/// generation (which changes whenever a genuinely new native slot is appended)
/// makes a stale negative self-heal at the cost of one `u32` compare, so the
/// mechanism is correct at every point in the VM's lifetime, not just after
/// boot. Positive entries are also revalidated, which is free.
///
/// # One cell, one triple — and how that is enforced
///
/// The memo is keyed on the registry generation **alone**. The
/// `(class, method, descriptor)` triple is deliberately *not* re-checked on a
/// warm hit: re-checking it would mean re-hashing the three strings, which is
/// the exact cost this type exists to remove. The invariant is therefore a
/// contract on the caller:
///
/// > **A given `NativeCallSite` must only ever be asked about one triple.**
///
/// Every intended embedding satisfies it structurally — a `static` cell beside
/// a constant-triple lookup, or a cell owned by the `CachedBytecodeMethod`
/// whose own `(class_name, method_name, method_descriptor)` is the triple being
/// looked up. The public API makes it hard to break by accident (there is no
/// way to inject a raw [`NativeMethodId`], and the strings are passed on every
/// call so a site cannot silently drift), but it cannot make it *impossible*:
/// sharing one `static` across two nearby call sites that happen to look up
/// different triples compiles fine and is silently wrong — the second site
/// redeems the first site's answer.
///
/// So that misuse is **loud instead of silent** in debug builds: the cell
/// remembers a digest of the first triple it was filled with and
/// `debug_assert!`s that every later query matches. The field and the check are
/// behind `#[cfg(debug_assertions)]`, so a release build is byte-for-byte the
/// one-word cell described above and pays nothing — deliberately, because a
/// release-path triple check would reinstate the string hashing.
pub struct NativeCallSite {
    memo: AtomicU64,
    /// Debug-only witness of "which triple has this cell been used for".
    ///
    /// `0` means "never queried"; any other value is a non-zero fold of the
    /// registry's own 128-bit digest of the triple. Not present in release
    /// builds. Relaxed ordering is sufficient: this is a debugging aid, and a
    /// benign race between two threads filling the *same* site with the *same*
    /// triple stores the same value.
    #[cfg(debug_assertions)]
    triple_witness: AtomicU64,
}

impl NativeCallSite {
    /// A fresh, unresolved call site.
    #[inline]
    pub const fn new() -> Self {
        Self {
            memo: AtomicU64::new(MEMO_EMPTY),
            #[cfg(debug_assertions)]
            triple_witness: AtomicU64::new(0),
        }
    }

    /// Record the triple this cell is being queried with and report whether it
    /// agrees with the one it was first queried with. Returns `true` for the
    /// first query, and `true` in release builds (where the witness field does
    /// not exist).
    ///
    /// Only ever called from inside a [`debug_assert!`], so in a release build
    /// the expression is not evaluated at all and the hash is never computed —
    /// which is the whole point of the memo. It must never become a
    /// release-path check.
    #[inline]
    #[cfg(debug_assertions)]
    fn witness_triple(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        let (lo, hi) = crate::registry::native_method_hash(class_name, method_name, descriptor);
        // Fold the registry's own 128-bit digest to one word and force it
        // non-zero, since 0 is the "never queried" sentinel.
        let digest = (lo ^ hi.rotate_left(32)) | 1;
        let prev = self.triple_witness.swap(digest, Ordering::Relaxed);
        prev == 0 || prev == digest
    }

    /// Release-build stand-in. Never reached from the hot path — the
    /// `debug_assert!` in [`check_one_triple_per_site`](Self::check_one_triple_per_site)
    /// does not evaluate its expression in release — so the triple is not
    /// hashed and the cell stays one word wide.
    #[inline]
    #[cfg(not(debug_assertions))]
    #[allow(dead_code)]
    fn witness_triple(&self, _class_name: &str, _method_name: &str, _descriptor: &str) -> bool {
        true
    }

    /// The `debug_assert!` wrapper around [`witness_triple`](Self::witness_triple).
    #[inline]
    fn check_one_triple_per_site(&self, class_name: &str, method_name: &str, descriptor: &str) {
        debug_assert!(
            self.witness_triple(class_name, method_name, descriptor),
            "NativeCallSite reused for a second (class, method, descriptor) triple \
             (now {}.{}{}). A call site memoizes ONE triple — the warm path never \
             re-checks the strings — so two distinct lookups must not share a cell. \
             Give each lookup its own `NativeCallSite`.",
            class_name,
            method_name,
            descriptor
        );
    }

    /// Resolve (and memoize) the native for this call site.
    ///
    /// Semantically identical to `registry.resolve_id(class, method, desc)` —
    /// including the descriptor-quirk fallback — but pays the three string
    /// hashes only on the first call and after a registration that appends a
    /// new slot.
    #[inline]
    pub fn resolve(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        self.check_one_triple_per_site(class_name, method_name, descriptor);
        let generation = registry.generation();
        let memo = self.memo.load(Ordering::Relaxed);
        if memo != MEMO_EMPTY && (memo >> 32) as u32 == generation {
            return Self::decode(memo);
        }
        self.fill(registry, generation, class_name, method_name, descriptor)
    }

    /// As [`resolve`](Self::resolve), but the caller supplies a digest computed
    /// once at class-link time (see [`NativeMethodKey`]). The strings are still
    /// required and still verified on a hit.
    #[inline]
    pub fn resolve_with_key(
        &self,
        registry: &NativeMethodRegistry,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        self.check_one_triple_per_site(class_name, method_name, descriptor);
        let generation = registry.generation();
        let memo = self.memo.load(Ordering::Relaxed);
        if memo != MEMO_EMPTY && (memo >> 32) as u32 == generation {
            return Self::decode(memo);
        }
        let id = registry.resolve_id_by_key(key, class_name, method_name, descriptor);
        self.store(generation, id);
        id
    }

    /// The resolved callback for this call site, or `None` if no native is
    /// registered for the triple. This is the drop-in replacement for
    /// `shared.natives.native_methods.find(class, method, desc)`.
    #[inline]
    pub fn callback(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        let id = self.resolve(registry, class_name, method_name, descriptor)?;
        registry.callback_of(id)
    }

    /// Drop-in replacement for `find_with_kind`: callback plus the category the
    /// native was registered under, from a single array index once warm.
    #[inline]
    pub fn callback_with_kind(
        &self,
        registry: &NativeMethodRegistry,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<(NativeCallback, NativeKind)> {
        let id = self.resolve(registry, class_name, method_name, descriptor)?;
        Some((registry.callback_of(id)?, registry.kind_of_id(id)?))
    }

    /// Forget the memoized result. Rarely needed — the generation check already
    /// handles new registrations — but available for a caller that knows its
    /// own resolution inputs changed.
    ///
    /// This clears the *memo*, not the cell's identity: the debug-only triple
    /// witness is deliberately retained, because invalidating a memo does not
    /// turn a call site into a different call site.
    #[inline]
    pub fn invalidate(&self) {
        self.memo.store(MEMO_EMPTY, Ordering::Relaxed);
    }

    /// Whether this call site has a memo valid for `registry`'s current
    /// generation. Diagnostics only; never a correctness input.
    #[inline]
    pub fn is_warm(&self, registry: &NativeMethodRegistry) -> bool {
        let memo = self.memo.load(Ordering::Relaxed);
        memo != MEMO_EMPTY && (memo >> 32) as u32 == registry.generation()
    }

    #[cold]
    #[inline(never)]
    fn fill(
        &self,
        registry: &NativeMethodRegistry,
        generation: u32,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        let id = registry.resolve_id(class_name, method_name, descriptor);
        self.store(generation, id);
        id
    }

    #[inline]
    fn store(&self, generation: u32, id: Option<NativeMethodId>) {
        // `slot + 1` so that 0 can mean "resolved, and there is no native".
        // `slots.len()` is bounded by the registration count (~3,100), so the
        // +1 cannot overflow in any realistic build; saturate rather than wrap
        // so a pathological registry degrades to "always re-resolve" instead of
        // aliasing slot 0.
        let encoded = match id {
            Some(id) => id.as_u32().saturating_add(1),
            None => 0,
        };
        self.memo.store(
            ((generation as u64) << 32) | encoded as u64,
            Ordering::Relaxed,
        );
    }

    #[inline]
    fn decode(memo: u64) -> Option<NativeMethodId> {
        let encoded = memo as u32;
        if encoded == 0 {
            None
        } else {
            Some(NativeMethodId::from_u32(encoded - 1))
        }
    }
}

impl Default for NativeCallSite {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// `CachedBytecodeMethod` derives `Clone`, so anything embedded in it must be
/// cloneable. Cloning copies the current memo: ids are registry-global, not
/// per-call-site, so the copy is valid for the same registry — and if it is
/// stale the generation check re-resolves it exactly as it would for a fresh
/// cell.
impl Clone for NativeCallSite {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            memo: AtomicU64::new(self.memo.load(Ordering::Relaxed)),
            // The clone stands at the same call site (a cloned
            // `CachedBytecodeMethod` describes the same method), so it inherits
            // the triple witness rather than starting blank — otherwise cloning
            // would launder a contract violation into a clean cell.
            #[cfg(debug_assertions)]
            triple_witness: AtomicU64::new(self.triple_witness.load(Ordering::Relaxed)),
        }
    }
}

impl std::fmt::Debug for NativeCallSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let memo = self.memo.load(Ordering::Relaxed);
        f.debug_struct("NativeCallSite")
            .field("generation", &((memo >> 32) as u32))
            .field("resolved", &Self::decode(memo))
            .field("warm", &(memo != MEMO_EMPTY))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::NativeContext;
    use cratonvm_types::error::MethodCallResult;
    use cratonvm_types::Value;

    fn native_a(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }

    fn native_b(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(7)))
    }

    fn addr(cb: NativeCallback) -> usize {
        cb as usize
    }

    #[test]
    fn warm_call_site_returns_the_same_callback() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "()V", native_a);
        let site = NativeCallSite::new();

        assert!(!site.is_warm(&registry));
        let first = site
            .callback(&registry, "a/A", "m", "()V")
            .expect("cold hit");
        assert!(site.is_warm(&registry));
        let second = site
            .callback(&registry, "a/A", "m", "()V")
            .expect("warm hit");
        assert_eq!(addr(first), addr(second));
        assert_eq!(
            addr(first),
            addr(registry.find("a/A", "m", "()V").expect("name lookup"))
        );
    }

    #[test]
    fn memoized_negative_self_heals_when_a_native_is_registered_later() {
        // The `OnceLock<Option<NativeCallback>>` this mechanism replaces caches
        // a `None` forever, which is only sound if registration is finished.
        // The generation check makes it sound at any point in the VM lifetime.
        let mut registry = NativeMethodRegistry::new();
        registry.register("other/O", "x", "()V", native_a);

        let site = NativeCallSite::new();
        assert!(site.callback(&registry, "late/L", "m", "()V").is_none());
        assert!(site.is_warm(&registry), "negative results are memoized too");

        registry.register("late/L", "m", "()V", native_b);
        assert!(
            !site.is_warm(&registry),
            "a new slot must invalidate the memo"
        );
        let cb = site
            .callback(&registry, "late/L", "m", "()V")
            .expect("stale negative must self-heal");
        assert_eq!(addr(cb), addr(native_b as NativeCallback));
    }

    #[test]
    fn re_registration_is_seen_through_a_warm_call_site() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "()V", native_a);
        let site = NativeCallSite::new();
        let before = site.callback(&registry, "a/A", "m", "()V").expect("hit");
        assert_eq!(addr(before), addr(native_a as NativeCallback));

        // Same triple, new callback: the slot is updated in place, so the warm
        // memo (which stores the slot index, not the callback) picks it up
        // without any invalidation.
        registry.register("a/A", "m", "()V", native_b);
        let after = site.callback(&registry, "a/A", "m", "()V").expect("hit");
        assert_eq!(addr(after), addr(native_b as NativeCallback));
    }

    #[test]
    fn call_site_agrees_with_direct_lookup_including_kind() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Intrinsic, |r| {
            r.register("java/lang/String", "length", "()I", native_b);
        });
        let site = NativeCallSite::new();
        let via_site = site
            .callback_with_kind(&registry, "java/lang/String", "length", "()I")
            .expect("hit");
        let direct = registry
            .find_with_kind("java/lang/String", "length", "()I")
            .expect("hit");
        assert_eq!(addr(via_site.0), addr(direct.0));
        assert_eq!(via_site.1, direct.1);
        assert_eq!(via_site.1, NativeKind::Intrinsic);
    }

    #[test]
    fn resolve_with_key_matches_resolve() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "(J)Ljava/lang/Object;", native_a);
        let key = NativeMethodKey::new("a/A", "m", "(J)Ljava/lang/Object;");

        let keyed = NativeCallSite::new();
        let named = NativeCallSite::new();
        assert_eq!(
            keyed.resolve_with_key(&registry, key, "a/A", "m", "(J)Ljava/lang/Object;"),
            named.resolve(&registry, "a/A", "m", "(J)Ljava/lang/Object;")
        );
        // Warm path too.
        assert_eq!(
            keyed.resolve_with_key(&registry, key, "a/A", "m", "(J)Ljava/lang/Object;"),
            named.resolve(&registry, "a/A", "m", "(J)Ljava/lang/Object;")
        );
    }

    #[test]
    fn invalidate_forces_a_re_resolve() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "()V", native_a);
        let site = NativeCallSite::new();
        assert!(site.callback(&registry, "a/A", "m", "()V").is_some());
        assert!(site.is_warm(&registry));
        site.invalidate();
        assert!(!site.is_warm(&registry));
        assert!(site.callback(&registry, "a/A", "m", "()V").is_some());
    }

    #[test]
    fn clone_preserves_the_memo() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "()V", native_a);
        let site = NativeCallSite::new();
        assert!(site.callback(&registry, "a/A", "m", "()V").is_some());
        let copy = site.clone();
        assert!(copy.is_warm(&registry));
        assert_eq!(
            copy.resolve(&registry, "a/A", "m", "()V"),
            site.resolve(&registry, "a/A", "m", "()V")
        );
    }

    #[test]
    fn a_memo_from_one_registry_is_not_honoured_by_another() {
        // The `static NativeCallSite` pattern the interpreter adoption plan
        // recommends is process-global, so it can be reached with a different
        // registry than it was filled from (test binaries build several VMs).
        // Generations are banded per registry, so this must re-resolve, not
        // redeem a slot index that means something else.
        let mut first = NativeMethodRegistry::new();
        first.register("a/A", "m", "()V", native_a);
        let mut second = NativeMethodRegistry::new();
        second.register("z/Z", "other", "()V", native_b);

        let site = NativeCallSite::new();
        assert!(site.callback(&first, "a/A", "m", "()V").is_some());
        assert!(site.is_warm(&first));
        assert!(
            !site.is_warm(&second),
            "a memo taken against one registry must not be warm for another"
        );
        // ...and re-resolving against the second registry gives the second
        // registry's (correct) answer, not the first's slot.
        assert!(site.callback(&second, "a/A", "m", "()V").is_none());

        // A `NativeCallSite` memoizes ONE call site, so the memo is keyed on the
        // registry generation alone — the triple is invariant by construction at
        // a real site and is deliberately not re-checked on a warm hit. Probing
        // a *different* triple therefore needs its own site: the line above just
        // memoized "not found" for `second`'s generation, and reusing `site`
        // here would redeem that negative rather than resolve `z/Z.other`.
        //
        // This is a live footgun for the interpreter adoption plan, which puts
        // `static NativeCallSite` cells at the constant-triple call sites: each
        // such site must have its own static, never a shared one.
        let other_site = NativeCallSite::new();
        assert!(other_site
            .callback(&second, "z/Z", "other", "()V")
            .is_some());
        // And that second site is likewise not warm for the first registry.
        assert!(!other_site.is_warm(&first));
    }

    // -----------------------------------------------------------------------
    // ARCH-2026-07-26 `cross-owner-closeout`: the one-cell-one-triple contract.
    //
    // The memo is keyed on the registry generation alone; the triple is NOT
    // re-checked on a warm hit (that would reinstate the string hashing this
    // type exists to remove). So a shared cell is silently wrong. These tests
    // pin the debug-only witness that makes it loud instead.
    // -----------------------------------------------------------------------

    #[test]
    fn witness_accepts_the_same_triple_repeatedly() {
        let site = NativeCallSite::new();
        for _ in 0..4 {
            assert!(site.witness_triple("a/A", "m", "()V"));
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    fn witness_rejects_a_second_triple_on_the_same_cell() {
        let site = NativeCallSite::new();
        assert!(
            site.witness_triple("a/A", "m", "()V"),
            "first query sets it"
        );
        assert!(
            !site.witness_triple("a/A", "m", "()I"),
            "a differing descriptor alone must be caught"
        );
        // The witness moves to the most recent triple, so a repeat of the new
        // one agrees — the assertion has already fired for the transition.
        assert!(site.witness_triple("a/A", "m", "()I"));
        assert!(
            !site.witness_triple("b/B", "m", "()I"),
            "a differing class must be caught"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "NativeCallSite reused for a second")]
    fn sharing_one_cell_between_two_triples_panics_in_debug() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "()V", native_a);
        registry.register("z/Z", "other", "()V", native_b);

        let shared = NativeCallSite::new();
        assert!(shared.callback(&registry, "a/A", "m", "()V").is_some());
        // Without the witness this would silently redeem `a/A.m`'s slot for
        // `z/Z.other` — the failure mode the invariant exists to prevent.
        let _ = shared.callback(&registry, "z/Z", "other", "()V");
    }

    #[test]
    fn a_dedicated_cell_per_triple_is_the_supported_shape() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "()V", native_a);
        registry.register("z/Z", "other", "()V", native_b);

        let site_a = NativeCallSite::new();
        let site_z = NativeCallSite::new();
        assert_eq!(
            addr(site_a.callback(&registry, "a/A", "m", "()V").expect("hit")),
            addr(native_a as NativeCallback)
        );
        assert_eq!(
            addr(
                site_z
                    .callback(&registry, "z/Z", "other", "()V")
                    .expect("hit")
            ),
            addr(native_b as NativeCallback)
        );
        // ...and stay correct once warm.
        assert_eq!(
            addr(site_a.callback(&registry, "a/A", "m", "()V").expect("hit")),
            addr(native_a as NativeCallback)
        );
        assert_eq!(
            addr(
                site_z
                    .callback(&registry, "z/Z", "other", "()V")
                    .expect("hit")
            ),
            addr(native_b as NativeCallback)
        );
    }

    #[test]
    fn invalidate_does_not_clear_the_triple_witness() {
        // `invalidate` forgets the *memo*; it does not turn the cell into a
        // different call site, so the contract still holds across it.
        let site = NativeCallSite::new();
        assert!(site.witness_triple("a/A", "m", "()V"));
        site.invalidate();
        assert!(site.witness_triple("a/A", "m", "()V"));
    }

    #[test]
    fn clone_carries_the_triple_witness() {
        let site = NativeCallSite::new();
        assert!(site.witness_triple("a/A", "m", "()V"));
        let copy = site.clone();
        assert!(copy.witness_triple("a/A", "m", "()V"));
        #[cfg(debug_assertions)]
        assert!(
            !copy.witness_triple("a/A", "m", "()I"),
            "cloning must not launder the contract into a blank cell"
        );
    }

    #[test]
    fn id_round_trips_through_a_raw_u32() {
        let id = NativeMethodId::from_u32(1234);
        assert_eq!(id.as_u32(), 1234);
        assert_eq!(id.index(), 1234usize);
        assert_eq!(NativeMethodId::from_u32(id.as_u32()), id);
    }
}
