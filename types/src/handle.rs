// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Rooted `Handle` type — the fix for the VM's #1 recurring bug family.
//!
//! `ObjectRef` (see [`crate::value::ObjectRef`]) is a raw pointer into the
//! GC heap. The moving collector relocates objects, so any native code that
//! copies an `ObjectRef` into a Rust local and then makes a re-entrant call
//! that can allocate (`invoke`, `new_object_initialized`, `new_array`, ...)
//! is holding a pointer the GC is free to invalidate mid-call. Every read of
//! that stale local afterward resolves to whatever now occupies the vacated
//! from-space slot — usually a reused, unrelated object — not the intended
//! one. `docs/known-issues/README.md` tracks 37+ independently-discovered,
//! independently-fixed sites in this exact shape; it is the long-tail bug
//! class of this codebase.
//!
//! The existing partial fix (`NativeContext::pin_native_root` /
//! `read_native_pin` / `unpin_native_roots`, `vm/src/vm/vm_exec.rs`) works
//! but puts the discipline entirely on the caller: pin before the call, read
//! back through the *pin handle* (not the original local) after, remember to
//! unpin. Nothing stops a native method from reading the pre-call local by
//! mistake — the type system doesn't distinguish "a raw `ObjectRef` copy"
//! from "a pinned one"; both are the same `ObjectRef` type.
//!
//! A [`RootedHandle`] closes that gap by construction: it does not store an
//! `ObjectRef` at all. It stores a slot id and reads *through* the slot on
//! every access via [`HandleStorage::get`] — so if a GC runs between rooting
//! and reading and updates the slot (the storage owner's job, mirroring how
//! `gc::update_all_roots` already rewrites `native_pin_roots` in place), the
//! handle's next read sees the current address automatically. There is no
//! "stale local" for a handle to be, because there is no local copy of the
//! pointer to go stale.
//!
//! # Why `HandleStorage` is a trait, defined here
//!
//! This crate (`cratonvm-types`) sits below `vm` in the dependency graph and
//! cannot see `JvmThread` or any GC machinery. The actual slot storage — a
//! per-thread `Vec<Option<ObjectRef>>` plus a scope-depth stack — lives on
//! `JvmThread` (`vm/src/threading/jvm_thread.rs`) and is rooted/remapped by
//! the GC (`vm/src/memory/roots.rs`, and see the follow-up note in
//! `docs/feature-designs/native-handle-discipline.md` about
//! `vm/src/memory/gc.rs`). [`HandleStorage`] is the narrow interface that
//! lets [`RootedHandle`]/[`HandleScope`] be defined — and unit-tested — here
//! without a circular `types -> vm` dependency, the same reason
//! `loader_pin`/`mirror_pin` live in this crate instead of `vm`.
//!
//! # Two layers
//!
//! * This module: a storage-generic, type-safe `RootedHandle`/`HandleScope`
//!   pair usable by any [`HandleStorage`] implementor (including the tests
//!   below, with no VM at all).
//! * `native_api::registry::NativeContext`: the VM-facing, trait-object-safe
//!   layer (`handle_scope_push`/`handle_scope_pop`/`handle_root`/
//!   `handle_get`) that native method implementations actually call. It
//!   deals in raw `u32` slots rather than `RootedHandle` because
//!   `&mut dyn NativeContext` can't hand back a `RootedHandle<'_, Self>`
//!   without naming `Self`, which a trait object erases. See that trait's
//!   doc comment for the discipline every native method must follow.

use crate::value::ObjectRef;

/// Storage backing rooted handles: mint a slot for an object, read the
/// current value at a slot, release a slot.
///
/// Implementors own the actual `Vec`/table and the GC-visibility contract
/// (a rooted-but-not-yet-`unroot`ed slot MUST be treated as a GC root, and
/// MUST be rewritten in place when its object moves) — this trait only
/// describes the shape callers need to root/read/release through.
pub trait HandleStorage {
    /// Root `r`, returning a fresh slot id. From this point until
    /// [`unroot`](Self::unroot) is called for the same slot, [`get`](Self::get)
    /// must return `r`'s CURRENT address — i.e. the implementor must treat
    /// the slot as a GC root and keep it updated across any collection that
    /// relocates the object.
    fn root(&mut self, r: ObjectRef) -> u32;

    /// Release `slot`. After this call, further reads of `slot` are
    /// implementation-defined (an implementor is free to reuse it) — callers
    /// must not still hold a [`RootedHandle`] wrapping it.
    fn unroot(&mut self, slot: u32);

    /// Current value at `slot` — the object's live, possibly-GC-forwarded
    /// address. Panics or returns a meaningless value for a `slot` that was
    /// never rooted or has already been [`unroot`](Self::unroot)ed;
    /// [`RootedHandle`] never calls this outside that contract.
    fn get(&self, slot: u32) -> ObjectRef;
}

/// A GC-safe handle to a heap object.
///
/// Unlike a bare [`ObjectRef`] copy, a `RootedHandle` never goes stale
/// across an allocating call: it holds only an opaque slot id and reads
/// *through* the owning [`HandleStorage`] on every access (via
/// [`get`](Self::get)), so a GC that relocates the object and updates the
/// slot is transparently reflected on the next read.
///
/// Deliberately not `Clone`/`Copy`: a slot is released exactly once (see
/// [`HandleScope::pop`]/`Drop`), so duplicating a `RootedHandle` would let a
/// copy outlive the slot it reads through, silently reading whatever the
/// storage later reuses that slot id for. Root the same object again
/// (`storage.root(handle.get(storage))`) if a second live handle to the same
/// object is genuinely needed.
#[derive(Debug)]
pub struct RootedHandle {
    slot: u32,
}

impl RootedHandle {
    /// Root `r` in `storage` and wrap the resulting slot.
    pub fn new<S: HandleStorage + ?Sized>(storage: &mut S, r: ObjectRef) -> Self {
        RootedHandle {
            slot: storage.root(r),
        }
    }

    /// Wrap an already-minted slot (e.g. one `NativeContext::handle_root`
    /// returned across the `native_api` trait-object boundary) without
    /// rooting again.
    pub fn from_slot(slot: u32) -> Self {
        RootedHandle { slot }
    }

    /// The opaque slot id, for passing back across an FFI/trait-object
    /// boundary that can't carry a borrowed `RootedHandle` directly (see the
    /// module doc's "two layers" note).
    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// Read the current (post-GC) reference through the slot. Never stale:
    /// this re-reads `storage` every call rather than returning a cached
    /// copy.
    pub fn get<S: HandleStorage + ?Sized>(&self, storage: &S) -> ObjectRef {
        storage.get(self.slot)
    }
}

/// RAII scope boundary over a [`HandleStorage`]: every [`RootedHandle`]
/// minted through [`root`](Self::root) is released, in LIFO order, when the
/// `HandleScope` is dropped (or explicitly via [`pop`](Self::pop)) — the
/// storage-generic analogue of `native_pin_roots`' "record a base index,
/// truncate back to it" discipline, expressed as a guard instead of a
/// manually-paired push/truncate call.
///
/// The VM's real integration (`NativeContext::handle_scope_push`/
/// `handle_scope_pop` on `NativeContextImpl`, `vm/src/vm/vm_exec.rs`) does
/// NOT build this generic guard on top of `HandleStorage` — a trait object
/// (`&mut dyn NativeContext`) can't easily hand back a lifetime-borrowing
/// guard tied to `Self`, so it re-implements the same base/truncate shape
/// directly against `JvmThread`'s slot `Vec`. `HandleScope` here exists so
/// the pattern is unit-testable, and usable, independent of the VM.
pub struct HandleScope<'s, S: HandleStorage + ?Sized> {
    storage: &'s mut S,
    /// Slots rooted through this scope, in mint order; unrooted in reverse
    /// on drop. (Not a single base index, unlike the VM's own
    /// `handle_scope_bases`, because `HandleStorage::unroot` only takes one
    /// slot at a time — see that method's doc comment.)
    rooted: Vec<u32>,
}

impl<'s, S: HandleStorage + ?Sized> HandleScope<'s, S> {
    /// Open a new scope over `storage`. Nothing is rooted yet.
    pub fn new(storage: &'s mut S) -> Self {
        HandleScope {
            storage,
            rooted: Vec::new(),
        }
    }

    /// Root `r` for the lifetime of this scope and return a handle to it.
    pub fn root(&mut self, r: ObjectRef) -> RootedHandle {
        let slot = self.storage.root(r);
        self.rooted.push(slot);
        RootedHandle { slot }
    }

    /// Read the current reference for a handle minted by this (or any)
    /// scope over the same `storage`.
    pub fn get(&self, handle: &RootedHandle) -> ObjectRef {
        self.storage.get(handle.slot)
    }

    /// Explicitly release every handle rooted through this scope, without
    /// waiting for `Drop`. Idempotent: a scope already popped (or never
    /// rooted anything) is a no-op.
    pub fn pop(&mut self) {
        while let Some(slot) = self.rooted.pop() {
            self.storage.unroot(slot);
        }
    }
}

impl<'s, S: HandleStorage + ?Sized> Drop for HandleScope<'s, S> {
    fn drop(&mut self) {
        self.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal in-memory `HandleStorage`: an append/tombstone `Vec`, no GC —
    /// enough to exercise the root/get/unroot contract in isolation.
    #[derive(Default)]
    struct TestStorage {
        slots: Vec<Option<ObjectRef>>,
    }

    fn fake_ref(addr: usize) -> ObjectRef {
        debug_assert!(addr != 0 && addr % 8 == 0);
        // SAFETY: non-null, 8-byte aligned; never dereferenced in tests,
        // only compared/round-tripped by pointer value.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    impl HandleStorage for TestStorage {
        fn root(&mut self, r: ObjectRef) -> u32 {
            self.slots.push(Some(r));
            (self.slots.len() - 1) as u32
        }
        fn unroot(&mut self, slot: u32) {
            if let Some(s) = self.slots.get_mut(slot as usize) {
                *s = None;
            }
        }
        fn get(&self, slot: u32) -> ObjectRef {
            self.slots[slot as usize].expect("read of an unrooted slot")
        }
    }

    #[test]
    fn rooted_handle_reads_through_storage() {
        let mut storage = TestStorage::default();
        let handle = RootedHandle::new(&mut storage, fake_ref(0x1000));
        assert_eq!(handle.get(&storage).as_ptr() as usize, 0x1000);
    }

    #[test]
    fn rooted_handle_sees_a_moved_object() {
        // Simulates what a GC does: root an object, then simulate a moving
        // collection by overwriting the slot in place (exactly what
        // `gc::update_all_roots` does to `native_pin_roots` after a move).
        // The handle sees the new address with no extra bookkeeping on the
        // caller's part — the entire point of reading through the slot
        // instead of caching the pointer.
        let mut storage = TestStorage::default();
        let handle = RootedHandle::new(&mut storage, fake_ref(0x1000));
        storage.slots[handle.slot() as usize] = Some(fake_ref(0x2000));
        assert_eq!(handle.get(&storage).as_ptr() as usize, 0x2000);
    }

    #[test]
    fn from_slot_wraps_an_externally_minted_slot() {
        let mut storage = TestStorage::default();
        let slot = storage.root(fake_ref(0x3000));
        let handle = RootedHandle::from_slot(slot);
        assert_eq!(handle.get(&storage).as_ptr() as usize, 0x3000);
    }

    #[test]
    fn handle_scope_pop_unroots_everything_rooted_through_it() {
        let mut storage = TestStorage::default();
        {
            let mut scope = HandleScope::new(&mut storage);
            let a = scope.root(fake_ref(0x1000));
            let b = scope.root(fake_ref(0x2000));
            assert_eq!(scope.get(&a).as_ptr() as usize, 0x1000);
            assert_eq!(scope.get(&b).as_ptr() as usize, 0x2000);
        } // scope drops here -> both slots unrooted
        assert!(storage.slots.iter().all(|s| s.is_none()));
    }

    #[test]
    fn handle_scope_pop_is_idempotent() {
        let mut storage = TestStorage::default();
        let mut scope = HandleScope::new(&mut storage);
        let _ = scope.root(fake_ref(0x1000));
        scope.pop();
        scope.pop(); // must not panic / double-free a slot
    }

    #[test]
    fn nested_scopes_unroot_independently() {
        let mut storage = TestStorage::default();
        let outer_handle;
        {
            let mut outer = HandleScope::new(&mut storage);
            outer_handle = outer.root(fake_ref(0x1000));
            {
                // Reborrow (not move) `outer.storage` so `outer` stays whole
                // once `inner` drops below.
                let mut inner = HandleScope::new(&mut *outer.storage);
                let inner_handle = inner.root(fake_ref(0x2000));
                assert_eq!(inner.get(&inner_handle).as_ptr() as usize, 0x2000);
            } // inner drops -> only its own slot is released
            assert_eq!(outer.get(&outer_handle).as_ptr() as usize, 0x1000);
        } // outer drops -> its slot is released too
        assert!(storage.slots.iter().all(|s| s.is_none()));
    }
}
