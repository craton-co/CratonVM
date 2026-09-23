// SPDX-License-Identifier: Apache-2.0
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

    /// Run `f` against `vm_identity`'s row if it exists, without creating one.
    ///
    /// Preferred for read-only paths — a GC root scan that used [`Self::with`]
    /// would resurrect a row for every VM that ever collected.
    pub fn peek<R>(&self, vm_identity: usize, f: impl FnOnce(&T) -> R) -> Option<R> {
        self.table().get(&vm_identity).map(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole type exists for: one VM cannot read or clobber
    /// another's row.
    #[test]
    fn rows_are_isolated_per_vm() {
        static T: VmScoped<Vec<u32>> = VmScoped::new();
        T.with(1, |row| row.push(10));
        T.with(2, |row| row.push(20));
        assert_eq!(T.peek(1, |r| r.clone()), Some(vec![10]));
        assert_eq!(T.peek(2, |r| r.clone()), Some(vec![20]));
        assert_eq!(T.peek(3, |r| r.clone()), None, "peek must not create a row");
        assert!(!T.has_row(3));
    }

    /// Teardown drops only the named VM — the shape `release_vm_native_state`
    /// relies on when one VM is disposed of while another is still running.
    #[test]
    fn forget_drops_one_row_only() {
        static T: VmScoped<Vec<u32>> = VmScoped::new();
        T.with(7, |row| row.push(1));
        T.with(8, |row| row.push(2));
        T.forget(7);
        assert!(!T.has_row(7));
        assert_eq!(T.peek(8, |r| r.clone()), Some(vec![2]));
        T.forget(7);
    }
}
