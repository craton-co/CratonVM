// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-global native side-tables, partitioned by VM.
//!
//! [`NativeContext::vm_identity`](crate::registry::NativeContext::vm_identity)
//! exists because a Rust test binary stands up many independent `Vm`s in one
//! process — sequentially in a `--test-threads=1` run, and *concurrently*
//! otherwise. Any native-side cache that stores heap `ObjectRef`s, or that is
//! keyed by something a VM mints from zero (a `ClassId`, an identity hash),
//! must therefore be scoped to the VM that filled it. An unscoped one is wrong
//! twice over:
//!
//! * **sequentially** — VM 2 allocates the same `ClassId`s VM 1 did, looks a
//!   key up, and gets VM 1's freed object back;
//! * **concurrently** — two live VMs interleave writes to the same cell, and
//!   whichever reads next dereferences an address in the other's heap. That is
//!   what made `cargo test -p cratonvm-vm --test interpreter_tests` die with a
//!   SIGSEGV in `gen_heap::class_id_of` unless it was run with
//!   `--test-threads=1`.
//!
//! [`VmScoped`] is the shape to reach for. It owns one `Mutex` over a
//! `HashMap<vm_identity, T>`; `with` hands out `&mut T` for the calling VM's
//! row only, so another VM's row is not addressable from a call site that has
//! a `ctx`. Pair every use with a `forget(vm_identity)` call from
//! `release_vm_native_state` (vm_init.rs) so a disposed VM's row — and the raw
//! heap addresses in it — do not outlive the heap that produced them.
//!
//! **Lock discipline:** `with` holds the table lock across the closure. Never
//! allocate, dispatch Java, or take a second `VmScoped` lock inside one:
//! GC root scans take these same locks at a safepoint, so a thread that
//! allocated while holding one could deadlock against its own collection.
//! Build the value first, publish it afterwards.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// A `HashMap<vm_identity, T>` behind one lazily-created `Mutex`.
///
/// Declare as a `static` — `new()` is `const`:
///
/// ```ignore
/// static PROXY_CACHE: VmScoped<FxHashMap<Key, ObjectRef>> = VmScoped::new();
/// ```
pub struct VmScoped<T> {
    cell: OnceLock<Mutex<HashMap<usize, T>>>,
}

impl<T> Default for VmScoped<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> VmScoped<T> {
    pub const fn new() -> Self {
        Self {
            cell: OnceLock::new(),
        }
    }

    fn table(&self) -> MutexGuard<'_, HashMap<usize, T>> {
        self.cell
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drop `vm_identity`'s row. Idempotent.
    pub fn forget(&self, vm_identity: usize) {
        self.table().remove(&vm_identity);
    }

    /// Whether `vm_identity` has a row at all. Cheap enough for a "is this
    /// table even in play for me" guard.
    pub fn has_row(&self, vm_identity: usize) -> bool {
        self.table().contains_key(&vm_identity)
    }
}

impl<T: Default> VmScoped<T> {
    /// Run `f` against `vm_identity`'s row, creating an empty one if needed.
    pub fn with<R>(&self, vm_identity: usize, f: impl FnOnce(&mut T) -> R) -> R {
        let mut table = self.table();
        f(table.entry(vm_identity).or_default())
    }

    /// [`Self::with`] for a caller inside a collection pause (gci i12/ul F5):
    /// takes the table lock only if it is free, and answers `None` (running
    /// nothing) when another thread -- a peer the pause froze, or this very
    /// thread further up its stack -- holds it. Never blocks.
    pub fn try_with<R>(&self, vm_identity: usize, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let mutex = self.cell.get_or_init(|| Mutex::new(HashMap::new()));
        let mut table = match mutex.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(f(table.entry(vm_identity).or_default()))
    }

    /// Run `f` against `vm_identity`'s row if it exists, without creating one.
    ///
    /// Preferred for read-only paths — a GC root scan that used [`Self::with`]
    /// would resurrect a row for every VM that ever collected.
    pub fn peek<R>(&self, vm_identity: usize, f: impl FnOnce(&T) -> R) -> Option<R> {
        self.table().get(&vm_identity).map(f)
    }
}

#[cfg(test)]
mod tests;
