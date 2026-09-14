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
    /// [`Self::get`]'s borrow depends on — literally true. Touched only on the
    /// STW unload path, so the mutex is never contended on an allocation.
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

    /// Unpublish the recipe for an unloaded class.
    ///
    /// Called only from the stop-the-world loader-unload transaction, after
    /// reachability proved that no instance or activation of the class remains.
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
    /// The STW argument does not rescue it. The unload transaction's guarantee
    /// is about *instances and activations of the unloaded class*, not about
    /// which line of `jit_post_alloc_init` some other thread is parked on; and
    /// the collector reaches its stop-the-world state partly by FORCIBLY
    /// freezing in-JIT peers (`stw_take_over_and_wait`), which stops a thread at
    /// an arbitrary instruction — including between `get()` and the
    /// `info.prim_inits` walk it feeds. Such a thread resumes and reads a freed
    /// `Box<[(u32, PrimKind)]>`: garbage field indices into `set_field` (dropped
    /// by its bounds check, but only after the allocator has reused the memory)
    /// and a garbage `has_finalizer` that can register an arbitrary object as
    /// finalizable.
    ///
    /// So the entry is unpublished (no later `get` can find it, which is all
    /// invalidation is FOR) and moved to `retired`, which `Drop` reclaims. The
    /// leak is one recipe per unloaded class — a `bool` plus a boxed slice of
    /// `(u32, PrimKind)` — bounded by classes actually unloaded, and paid only
    /// on a path that already takes a VM-wide write lock.
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
        };
        let b = ClassAllocInfo {
            has_finalizer: true,
            prim_inits: Box::new([(1, PrimKind::Float)]),
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
