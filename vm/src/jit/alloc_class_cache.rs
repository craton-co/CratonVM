// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-class allocation-init cache for the JIT slow-path allocators.
//!
//! Every slow-path object allocation (`jit_new_object` and the guarded
//! TLAB-refill arm) used to pay a `class_manager.read()` + full superclass
//! hierarchy walk in `jit_init_primitive_fields`, plus a SECOND
//! `class_manager.read()` + class_store lookup for the `has_finalizer`
//! registration check. The compile-time resolver (`resolve_jit_new_site`)
//! already proves this metadata is stable by the time a `new` executes — it
//! bakes `(num_fields, has_primitive_init, has_finalizer)` into the emitted
//! code for the inline-TLAB fast path. This cache extends the same stability
//! assumption to the helper path: the first slow-path allocation of a class
//! computes a compact init recipe (primitive-field slots + finalizer flag)
//! under the read lock, publishes it lock-free, and every later allocation
//! of that class skips the class manager entirely.
//!
//! Layout: a two-level table indexed by `ClassId` (dense, allocator-issued —
//! same assumption as `cratonvm_types::CLASS_LAYOUTS`). The top level is a
//! fixed array of `AtomicPtr<Chunk>`; chunks hold `AtomicPtr<ClassAllocInfo>`
//! entries published with a `null -> ptr` CAS. Entries are immutable once
//! published and freed only on VM drop, so `get` can hand out `&` borrows
//! tied to the cache's lifetime — see [`JitAllocClassCache::invalidate`], which
//! unpublishes without freeing precisely to keep that sentence true. The cache
//! lives on `SharedVm` (NOT a process
//! global): it is populated lazily from `ClassManager` state, and a process
//! global would serve stale entries to a second VM in the same test process.
//!
//! Opt out with `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE=1` (presence check, like
//! `CRATONVM_NO_PRECISE_JIT_MAPS`): the helpers then fall back to the legacy
//! per-allocation class-manager lookups.
//!
//! # The interpreter's `new` reads it too (round i1 wave 3)
//!
//! `gc_and_alloc::gc_alloc_object` — the interpreter's `new` after the site
//! cache — used to take the class-manager read lock and walk the whole
//! superclass chain on EVERY allocation to write the JVM defaults and read
//! `has_finalizer`: an atomic RMW on a lock word every allocating thread
//! shares, and a wait behind any thread defining a class. It now reads the
//! same recipe (building and publishing it on a miss, through the one builder
//! [`ClassAllocInfo::build`]), so a warm interpreted `new` takes no lock. For
//! that the recipe also lists the REFERENCE slots ([`ClassAllocInfo::ref_inits`]):
//! the interpreter writes `Object(None)` into them, as its locked walk did.
//! The JIT helper (`jit::helpers::jit_post_alloc_init`) still writes
//! only `prim_inits`, and its inline-TLAB skip gate still keys on
//! `prim_inits` alone — see `jit_init_primitive_fields` for why widening
//! either is a separate, measured change. Since round i1 wave 5 both readers
//! skip the `PrimKind::Int` entries (every allocator hands out a zeroed body,
//! which already reads `Int(0)`), so for the skip gate only `J`/`F`/`D`
//! entries and the finalizer flag count — the rule `jit_new_site_flags`
//! applies at compile time. The one weakness the recipe has and
//! the locked walk did not — an allocation that read the recipe just before an
//! [`JitAllocClassCache::invalidate`] writes defaults at the OLD indices — is
//! the one this cache already accepted for the JIT; see there.
//!
//! # Per-VM state audit: BENIGN — already correct, do not re-litigate
//!
//! The 2026-08-01 `vm/src/jit/` cache-keying sweep
//! (`vm-jit-cache-keying.md`) checked this module and found
//! nothing to fix. The table is `ClassId`-keyed, which is per-VM state, but it
//! is NOT a process global: it is a by-value field of `JitRealm` inside
//! `Arc<SharedVm>` (`vm/src/vm/realms/jit_realm.rs` `jit_alloc_class_cache`,
//! constructed at `vm/src/vm/vm_init.rs`), so each VM has its own and the
//! `ClassId` index is unambiguous within it. The paragraph above says as much;
//! this note records that the claim was verified rather than assumed. It also
//! holds no `ObjectRef` and no heap address — only field indices, a `PrimKind`
//! and a `bool` — so it is not a GC-root concern either.

use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;

/// Primitive-field kind for default-value initialization (JLS §4.12.5).
/// `Int` covers all int-family descriptors (`I B C S Z`) — the interpreter's
/// `jit_init_primitive_fields` stores `Value::Int(0)` for all five.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimKind {
    Int,
    Long,
    Float,
    Double,
}

impl PrimKind {
    /// The kind of a field whose descriptor starts with `desc_first`, or
    /// `None` for a reference (and for any malformed byte, which
    /// `gc_and_alloc::jvm_default_for_descriptor` defaults to `null`).
    #[inline]
    pub fn of_descriptor(desc_first: u8) -> Option<Self> {
        match desc_first {
            b'I' | b'B' | b'C' | b'S' | b'Z' => Some(PrimKind::Int),
            b'J' => Some(PrimKind::Long),
            b'F' => Some(PrimKind::Float),
            b'D' => Some(PrimKind::Double),
            _ => None,
        }
    }

    /// The JVM default value of this kind (JVMS §2.3).
    #[inline]
    pub fn default_value(self) -> cratonvm_types::Value {
        use cratonvm_types::Value;
        match self {
            PrimKind::Int => Value::Int(0),
            PrimKind::Long => Value::Long(0),
            PrimKind::Float => Value::Float(0.0),
            PrimKind::Double => Value::Double(0.0),
        }
    }
}

/// Immutable per-class allocation-init recipe.
#[derive(Debug)]
pub struct ClassAllocInfo {
    /// Whether the class (or a superclass) overrides `finalize()` — gates
    /// `register_finalizable` (JLS §12.6).
    pub has_finalizer: bool,
    /// `(absolute field index, kind)` of every non-static primitive field in
    /// the hierarchy, in the same walk order as `jit_init_primitive_fields`.
    /// Empty when the class needs no primitive default-init.
    pub prim_inits: Box<[(u32, PrimKind)]>,
    /// Absolute field index of every other non-static field in the hierarchy —
    /// reference and array descriptors, and any malformed descriptor byte —
    /// whose JVM default is a WRITTEN `Object(None)` (a zeroed slot decodes as
    /// `Int(0)`; see `gc_and_alloc::init_primitive_fields`). Read by the
    /// interpreter's `new`; the JIT helper does not write these (see the
    /// module docs), and nothing that gates the JIT's fast path may key on it.
    pub ref_inits: Box<[u32]>,
}

impl ClassAllocInfo {
    /// Build the recipe for `class_id` from `store`: the superclass-chain walk
    /// of `gc_and_alloc::write_default_instance_fields` and
    /// `jit_init_primitive_fields`, split by default kind. `None` — publish
    /// nothing, take the caller's locked walk — when `class_id` or any class
    /// in its superclass chain is missing from the store, so only a COMPLETE
    /// recipe is ever cached.
    ///
    /// The one builder for both readers, so the interpreter's and the JIT's
    /// view of a class's defaults cannot drift apart.
    ///
    /// # A compatibility stub in the chain is never cached (gc-common w9-f)
    ///
    /// Also `None` when `class_id` or any superclass is a
    /// `ClassOrigin::CompatibilityStub`. `upgrade_synthetic_class` rewrites a
    /// stub's `fields` IN PLACE when its real bytes arrive, and a recipe
    /// published before that keeps the stub's descriptors. The invalidation
    /// hook reaches the upgraded class itself and every descendant whose
    /// `first_field_index` / `num_total_fields` moved, but NOT a descendant
    /// whose layout numbers stayed put: the synthetic field-count floor
    /// (`apply_synthetic_floor`) keeps the parent's total, so its subclasses
    /// are never recomputed, while the parent's slots change kind
    /// (`J` -> `L`, say). Such a subclass then kept writing the stub's
    /// defaults -- a `Long(0)` into a reference slot, or no store at all where
    /// the real field needs the `Object(None)` tag -- on every `new`, forever.
    /// The classic locked walk reads the live fields each time, so it is always
    /// right; a stub chain takes it, as the interpreter's (since retired)
    /// per-thread plan cache already refused to plan one. No
    /// class in `--jdk-only` has a stub in its chain, so the refusal costs
    /// nothing there; in `--compatible` it costs one class-manager read per
    /// allocation of a stub-descended class, the pre-cache behaviour.
    pub fn build(
        store: &crate::classloading::ClassStore,
        class_id: crate::classloading::ClassId,
    ) -> Option<Self> {
        let root = store.get(class_id)?;
        let has_finalizer = root.has_finalizer;
        // Pass 1, allocation-free: refuse before building anything. A refused
        // class is never published, so EVERY `new` of it asks again (the
        // interpreter's `gc_alloc_object` and the JIT helper both do); this
        // pass keeps that repeated refusal from allocating and freeing the
        // partial lists on each of those allocations (round i1 wave 16). It
        // also counts the instance fields, so pass 2 allocates each list once.
        let mut prim_count = 0usize;
        let mut ref_count = 0usize;
        let mut cid = Some(class_id);
        let mut depth = 0usize;
        while let Some(current_id) = cid {
            let class = store.get(current_id)?;
            // See the doc above: a stub's layout can change under a published
            // recipe without anything unpublishing it.
            if class.origin.is_compatibility_stub() {
                return None;
            }
            // A malformed (cyclic) superclass chain must not hang an
            // allocation; the locked walk it replaces is the fallback.
            depth += 1;
            if depth > 4096 {
                return None;
            }
            for f in class.fields.iter().filter(|f| !f.is_static()) {
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                if PrimKind::of_descriptor(desc_first).is_some() {
                    prim_count += 1;
                } else {
                    ref_count += 1;
                }
            }
            cid = class.superclass;
        }
        // Pass 2: the same chain (the store is borrowed immutably throughout,
        // so it cannot have changed), filled in walk order.
        let mut prim_inits: Vec<(u32, PrimKind)> = Vec::with_capacity(prim_count);
        let mut ref_inits: Vec<u32> = Vec::with_capacity(ref_count);
        let mut cid = Some(class_id);
        while let Some(current_id) = cid {
            let class = store.get(current_id)?;
            let mut inst_idx = class.first_field_index;
            for f in &class.fields {
                if f.is_static() {
                    continue;
                }
                // Truncation-checked: field indices are bounded by
                // `num_total_fields`, far below u32::MAX.
                let idx = u32::try_from(inst_idx).ok()?;
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                match PrimKind::of_descriptor(desc_first) {
                    Some(kind) => prim_inits.push((idx, kind)),
                    None => ref_inits.push(idx),
                }
                inst_idx += 1;
            }
            cid = class.superclass;
        }
        Some(ClassAllocInfo {
            has_finalizer,
            prim_inits: prim_inits.into_boxed_slice(),
            ref_inits: ref_inits.into_boxed_slice(),
        })
    }
}

/// Entries per chunk / chunks in the top level. `TOP_LEN * CHUNK_LEN` covers
/// the same id range as `cratonvm_types::MAX_DENSE_CLASS_LAYOUTS` (1<<20);
/// ids beyond that fall back to the uncached path instead of allocating
/// sparsely.
const CHUNK_LEN: usize = 1024;
const TOP_LEN: usize = 1024;

struct Chunk {
    entries: [AtomicPtr<ClassAllocInfo>; CHUNK_LEN],
}

impl Chunk {
    fn new_boxed() -> Box<Chunk> {
        Box::new(Chunk {
            entries: [const { AtomicPtr::new(std::ptr::null_mut()) }; CHUNK_LEN],
        })
    }
}

/// Lock-free `ClassId -> ClassAllocInfo` side table (see module docs).
pub struct JitAllocClassCache {
    chunks: Box<[AtomicPtr<Chunk>; TOP_LEN]>,
    /// Entries unpublished by [`Self::invalidate`], held until `Drop`.
    ///
    /// Keeps "published entries are freed only on VM drop" — the invariant
    /// [`Self::get`]'s borrow depends on — literally true. Touched only by
    /// [`Self::invalidate`] (class unload and layout-change invalidation), so
    /// the mutex is never taken on an allocation.
    retired: std::sync::Mutex<Vec<*mut ClassAllocInfo>>,
}

// SAFETY: `retired` holds `*mut ClassAllocInfo` only as deferred-free
// bookkeeping — the pointers are never dereferenced while in the vector, and
// `ClassAllocInfo` is `Send + Sync` by construction (a `bool` and a boxed slice
// of `Copy` scalars). The rest of the cache is already shared across threads
// through atomics.
unsafe impl Send for JitAllocClassCache {}
unsafe impl Sync for JitAllocClassCache {}

impl Default for JitAllocClassCache {
    fn default() -> Self {
        Self::new()
    }
}

impl JitAllocClassCache {
    pub fn new() -> Self {
        JitAllocClassCache {
            chunks: Box::new([const { AtomicPtr::new(std::ptr::null_mut()) }; TOP_LEN]),
            retired: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Cached recipe for `class_id`, or `None` if not yet published (or the
    /// id is outside the dense range).
    #[inline]
    pub fn get(&self, class_id: u32) -> Option<&ClassAllocInfo> {
        let idx = class_id as usize;
        let chunk = self.chunks.get(idx / CHUNK_LEN)?.load(Ordering::Acquire);
        if chunk.is_null() {
            return None;
        }
        // SAFETY: a non-null chunk pointer was published by `insert` via a
        // Release CAS and is only freed in `Drop` (which takes `&mut self`,
        // so no `&self` borrow can be live).
        let entry = unsafe { &(*chunk).entries[idx % CHUNK_LEN] }.load(Ordering::Acquire);
        if entry.is_null() {
            None
        } else {
            // SAFETY: same publication/immutability argument as the chunk.
            Some(unsafe { &*entry })
        }
    }

    /// Publish the recipe for `class_id` (first writer wins; racing inserts
    /// return the winner's entry). Returns `None` — without publishing —
    /// when `class_id` is outside the dense range.
    pub fn insert(&self, class_id: u32, info: ClassAllocInfo) -> Option<&ClassAllocInfo> {
        let idx = class_id as usize;
        let chunk_slot = self.chunks.get(idx / CHUNK_LEN)?;
        let mut chunk = chunk_slot.load(Ordering::Acquire);
        if chunk.is_null() {
            let fresh = Box::into_raw(Chunk::new_boxed());
            match chunk_slot.compare_exchange(
                std::ptr::null_mut(),
                fresh,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => chunk = fresh,
                Err(winner) => {
                    // SAFETY: `fresh` was just created above, never published.
                    drop(unsafe { Box::from_raw(fresh) });
                    chunk = winner;
                }
            }
        }
        // SAFETY: chunk is non-null and published (see `get`).
        let entry_slot = unsafe { &(*chunk).entries[idx % CHUNK_LEN] };
        let fresh = Box::into_raw(Box::new(info));
        match entry_slot.compare_exchange(
            std::ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: we just published `fresh`; it stays live until Drop.
            Ok(_) => Some(unsafe { &*fresh }),
            Err(winner) => {
                // SAFETY: `fresh` lost the race and was never published.
                drop(unsafe { Box::from_raw(fresh) });
                // SAFETY: `winner` is a published entry (see `get`).
                Some(unsafe { &*winner })
            }
        }
    }

    /// Unpublish the recipe for a class whose field layout may have changed.
    ///
    /// # Who calls this, and why the answer matters
    ///
    /// There are two callers:
    ///
    /// * `jit_invalidate_adapter` (`vm/src/vm/vm_init.rs`), the `ClassInfoHook`
    ///   the GC fires on an **in-place layout change**: a class redefinition, a
    ///   subclass re-layout, or a synthetic stub being replaced by real bytecode
    ///   with a different field count or order. It fans out over every live
    ///   VM, because the hook carries no VM identity.
    /// * the class-UNLOAD path in `vm/src/memory/gc.rs`, once per unloaded
    ///   class. `ClassId`s are NOT recycled today (`ClassStore::add` appends,
    ///   `ClassStore::remove` leaves a tombstone), so this call only releases
    ///   the dead class's recipe; it becomes load-bearing if ids are ever
    ///   reused, when the next class issued the id would otherwise inherit the
    ///   dead class's primitive-field recipe and `has_finalizer` bit.
    ///
    /// (This paragraph used to say the adapter was the ONLY caller; the unload
    /// call was not listed. It also said ids were recycled, which they are not.)
    ///
    /// This comment used to say "called only from the stop-the-world
    /// loader-unload transaction, after reachability proved that no instance or
    /// activation of the class remains". Both halves were wrong about the
    /// caller that exists, and the second half was the load-bearing one: on a
    /// redefinition, instances and activations of the class emphatically DO
    /// remain — that is the whole point of a redefinition — so no argument that
    /// rests on their absence may be made here. Nothing below rests on one; the
    /// deferred free is what makes this sound, and it is sound for either
    /// caller.
    ///
    /// The contract for the caller is therefore just this: after this returns,
    /// the next slow-path allocation of `class_id` rebuilds the recipe from the
    /// current layout. An allocation already past its `get()` keeps the recipe
    /// it read, which for a layout change means primitive default-init at the
    /// OLD field indices of a freshly zeroed object — bounded by `set_field`'s
    /// own range check, and a no-op for any index the new layout still has,
    /// since the object's slots are already zero.
    ///
    /// # Why this does not free the entry
    ///
    /// [`Self::get`] returns `&ClassAllocInfo` borrowed from `&self`, and its
    /// SAFETY comment justifies that with "entries ... are only freed in `Drop`
    /// (which takes `&mut self`)". This method used to `drop(Box::from_raw(..))`
    /// immediately, which made that sentence false and the borrow a
    /// use-after-free — the reader holds a plain `&`, so nothing at the type
    /// level connects it to the swap here.
    ///
    /// No stop-the-world argument rescues it, and the actual caller does not
    /// even offer one: `jit_invalidate_adapter` runs on whatever thread the
    /// `ClassInfoHook` fires on, with other threads running. Even under a real
    /// STW the guarantee would be about *instances and activations of the
    /// class*, not about which line of `jit_post_alloc_init` some other thread
    /// is parked on; and the collector reaches its stop-the-world state partly
    /// by FORCIBLY freezing in-JIT peers (`stw_take_over_and_wait`), which stops
    /// a thread at an arbitrary instruction — including between `get()` and the
    /// `info.prim_inits` walk it feeds. Such a thread resumes and reads a freed
    /// `Box<[(u32, PrimKind)]>`: garbage field indices into `set_field` (dropped
    /// by its bounds check, but only after the allocator has reused the memory)
    /// and a garbage `has_finalizer` that can register an arbitrary object as
    /// finalizable.
    ///
    /// So the entry is unpublished (no later `get` can find it, which is all
    /// invalidation is FOR) and moved to `retired`, which `Drop` reclaims. The
    /// leak is one recipe per invalidation — a `bool` plus a boxed slice of
    /// `(u32, PrimKind)` — bounded by layout changes actually performed, and
    /// paid only on a path that already evicts the whole JIT method cache.
    pub fn invalidate(&self, class_id: u32) -> bool {
        let idx = class_id as usize;
        let Some(chunk_slot) = self.chunks.get(idx / CHUNK_LEN) else {
            return false;
        };
        let chunk = chunk_slot.load(Ordering::Acquire);
        if chunk.is_null() {
            return false;
        }
        // SAFETY: published chunks live until cache Drop.
        let entry = unsafe { &(*chunk).entries[idx % CHUNK_LEN] }
            .swap(std::ptr::null_mut(), Ordering::AcqRel);
        if entry.is_null() {
            false
        } else {
            // Ownership moves to `retired`, NOT to a `drop` here: a concurrent
            // (or forcibly-frozen) reader may still hold a `&` to it. See the
            // doc comment above.
            self.retired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(entry);
            true
        }
    }
}

impl Drop for JitAllocClassCache {
    fn drop(&mut self) {
        for chunk_slot in self.chunks.iter() {
            let chunk = chunk_slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
            if chunk.is_null() {
                continue;
            }
            // SAFETY: exclusive access in Drop; pointers came from Box::into_raw.
            let chunk = unsafe { Box::from_raw(chunk) };
            for entry_slot in chunk.entries.iter() {
                let entry = entry_slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
                if !entry.is_null() {
                    // SAFETY: as above.
                    drop(unsafe { Box::from_raw(entry) });
                }
            }
        }
        // The deferred-free list from `invalidate`. `&mut self` here means no
        // `get` borrow can still be live, which is exactly the condition that
        // was missing at the `invalidate` call site.
        for entry in self
            .retired
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            // SAFETY: unpublished by `invalidate` (so unreachable via `get`)
            // and owned by this list ever since; pointer came from
            // `Box::into_raw` in `insert`.
            drop(unsafe { Box::from_raw(entry) });
        }
    }
}

/// Whether the cache is enabled.
///
/// Enabled by default. Opt out with `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE=1`.
/// Fresh isolated bintrees measurements show that removing the repeated class
/// manager lookup materially complements the inline allocation path; cache
/// entries are immutable and scoped to a `SharedVm`, so this does not weaken
/// allocation or class-lifetime correctness.
#[inline]
pub fn alloc_class_cache_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !cratonvm_types::flags::runtime_flag_on("CRATONVM_NO_JIT_ALLOC_CLASS_CACHE"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_empty_returns_none() {
        let cache = JitAllocClassCache::new();
        assert!(cache.get(0).is_none());
        assert!(cache.get(12345).is_none());
        assert!(cache.get(u32::MAX).is_none());
    }

    #[test]
    fn insert_then_get_roundtrip() {
        let cache = JitAllocClassCache::new();
        let info = ClassAllocInfo {
            has_finalizer: true,
            prim_inits: vec![(3, PrimKind::Int), (7, PrimKind::Long)].into_boxed_slice(),
            ref_inits: Box::new([]),
        };
        let published = cache.insert(42, info).unwrap();
        assert!(published.has_finalizer);
        assert_eq!(
            published.prim_inits.as_ref(),
            &[(3, PrimKind::Int), (7, PrimKind::Long)]
        );
        let got = cache.get(42).unwrap();
        assert!(got.has_finalizer);
        assert_eq!(got.prim_inits.len(), 2);
        // Neighbors untouched.
        assert!(cache.get(41).is_none());
        assert!(cache.get(43).is_none());
    }

    #[test]
    fn first_insert_wins() {
        let cache = JitAllocClassCache::new();
        let a = ClassAllocInfo {
            has_finalizer: false,
            prim_inits: Box::new([]),
            ref_inits: Box::new([]),
        };
        let b = ClassAllocInfo {
            has_finalizer: true,
            prim_inits: Box::new([(1, PrimKind::Float)]),
            ref_inits: Box::new([]),
        };
        cache.insert(7, a);
        let winner = cache.insert(7, b).unwrap();
        assert!(!winner.has_finalizer, "first published entry must win");
        assert!(cache.get(7).unwrap().prim_inits.is_empty());
    }

    #[test]
    fn out_of_range_id_is_uncached() {
        let cache = JitAllocClassCache::new();
        let info = ClassAllocInfo {
            has_finalizer: false,
            prim_inits: Box::new([]),
            ref_inits: Box::new([]),
        };
        assert!(cache.insert(u32::MAX, info).is_none());
        assert!(cache.get(u32::MAX).is_none());
    }

    #[test]
    fn cross_chunk_ids() {
        let cache = JitAllocClassCache::new();
        for id in [0u32, 1023, 1024, 1025, 999_999] {
            let info = ClassAllocInfo {
                has_finalizer: id % 2 == 0,
                prim_inits: Box::new([(id, PrimKind::Double)]),
                ref_inits: Box::new([]),
            };
            cache.insert(id, info);
        }
        for id in [0u32, 1023, 1024, 1025, 999_999] {
            let got = cache.get(id).unwrap();
            assert_eq!(got.has_finalizer, id % 2 == 0);
            assert_eq!(got.prim_inits.as_ref(), &[(id, PrimKind::Double)]);
        }
    }

    /// `invalidate` must unpublish WITHOUT freeing: a reader that already has
    /// the `&` — a thread frozen between `get()` and its `prim_inits` walk — is
    /// unreachable from here and must not have its recipe reclaimed underneath.
    ///
    /// This asserts on the entry's CONTENT after the invalidate, so it fails
    /// (loudly under Miri/ASan, and by mismatch once the allocator reuses the
    /// block) if the entry is freed rather than retired. It also fixes the
    /// direction of the other half: `get` must report the class as gone.
    #[test]
    fn invalidate_unpublishes_but_does_not_free_a_live_borrow() {
        let cache = JitAllocClassCache::new();
        cache.insert(
            9,
            ClassAllocInfo {
                has_finalizer: true,
                prim_inits: vec![(11, PrimKind::Long), (12, PrimKind::Double)].into_boxed_slice(),
                ref_inits: Box::new([]),
            },
        );
        // Stand in for the frozen reader: take the borrow BEFORE invalidating.
        let borrowed = cache.get(9).expect("published");
        assert!(
            cache.invalidate(9),
            "entry was published, so it unpublishes"
        );
        assert!(cache.get(9).is_none(), "invalidate must unpublish");
        assert!(borrowed.has_finalizer);
        assert_eq!(
            borrowed.prim_inits.as_ref(),
            &[(11, PrimKind::Long), (12, PrimKind::Double)],
            "a borrow taken before invalidate must still read its own recipe",
        );
    }

    #[test]
    fn invalidate_is_idempotent_and_reinsertable() {
        let cache = JitAllocClassCache::new();
        cache.insert(
            5,
            ClassAllocInfo {
                has_finalizer: false,
                prim_inits: Box::new([(1, PrimKind::Int)]),
                ref_inits: Box::new([]),
            },
        );
        assert!(cache.invalidate(5));
        assert!(
            !cache.invalidate(5),
            "second invalidate has nothing to take"
        );
        // The id is free again: a reloaded class with the same id must be able
        // to publish a fresh recipe rather than inherit the retired one.
        let fresh = cache
            .insert(
                5,
                ClassAllocInfo {
                    has_finalizer: true,
                    prim_inits: Box::new([(2, PrimKind::Float)]),
                    ref_inits: Box::new([]),
                },
            )
            .expect("slot is empty again");
        assert!(fresh.has_finalizer);
        assert_eq!(
            cache.get(5).unwrap().prim_inits.as_ref(),
            &[(2, PrimKind::Float)]
        );
    }

    #[test]
    fn concurrent_insert_get_smoke() {
        use std::sync::Arc;
        let cache = Arc::new(JitAllocClassCache::new());
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || {
                    for i in 0..2000u32 {
                        let id = i % 512;
                        if let Some(info) = cache.get(id) {
                            // Whoever won, the entry must be self-consistent.
                            assert_eq!(info.prim_inits.len(), 1);
                            assert_eq!(info.prim_inits[0].0, id);
                        } else {
                            let _ = cache.insert(
                                id,
                                ClassAllocInfo {
                                    has_finalizer: t % 2 == 0,
                                    prim_inits: Box::new([(id, PrimKind::Int)]),
                                    ref_inits: Box::new([]),
                                },
                            );
                        }
                    }
                })
            })
            .collect();
        for th in threads {
            th.join().unwrap();
        }
        for id in 0..512u32 {
            assert_eq!(cache.get(id).unwrap().prim_inits[0].0, id);
        }
    }
}
