// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2 (lane VHCOMP; page
//! `docs/internal/jit/jr26-static-varhandle-rmw-is-a-native-call-FIXED-20261029.md`,
//! `CRATONVM_JIT_VH_CONST_STATIC`): this VM's cells for the compiled static
//! constant-`VarHandle` sites (`cratonvm_jit::ir_lower::vh_static_const`, whose
//! module comment says what compiled code does with one).
//!
//! * **Registered** when a class finishes `<clinit>`: one unfilled cell per
//!   `static final VarHandle` field of it that filed no instance or array
//!   plan (`note_class_initialized_varhandles`). A compile may bake the
//!   address from then on; an unfilled cell declines to the call.
//! * **Filled** on the mutator by [`note_target`], which the natives' static
//!   arms call when they resolve a handle for an initialized holder: every
//!   registered field of an initialized class that holds that very handle
//!   object is settled -- filled with the target static when the handle is
//!   writable and the static is a whole `int` / `long` the hardware CAS
//!   serves, left unfilled for good otherwise.
//! * **Cleared** when a class it names (the holder of the handle or of the
//!   static) unloads. A cell is never freed: compiled code may still hold its
//!   address.

use crate::classloading::ClassId;
use crate::vm::SharedVm;
use cratonvm_jit::ir_lower::vh_static_const::{vh_const_static_enabled, VhStaticCell};
use cratonvm_types::{ObjectRef, Value};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The static one handle names, as a cell is filled with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhStaticTarget {
    pub(crate) base_cell: usize,
    pub(crate) field_index: usize,
    pub(crate) kind: u8,
    pub(crate) class_id: u32,
}

struct Row {
    cell: Box<VhStaticCell>,
    /// The target static's class once filled, `0` before.
    target_class: u32,
}

#[derive(Default)]
struct Inner {
    rows: FxHashMap<(u32, u32), Row>,
    /// Registered keys no note has settled yet.
    unresolved: Vec<(u32, u32)>,
    /// Cells of unloaded classes: cleared, kept alive for compiled code.
    retired: Vec<Box<VhStaticCell>>,
}

/// See the module comment.
#[derive(Default)]
pub(crate) struct VhStaticCells {
    inner: Mutex<Inner>,
    /// `inner.unresolved.len()`, read without the lock by [`note_target`].
    unresolved: AtomicUsize,
}

impl VhStaticCells {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register the `static final VarHandle` fields `(class, index)`.
    pub(crate) fn register(&self, keys: &[(u32, u32)]) {
        let mut inner = self.inner.lock();
        for &key in keys {
            if inner.rows.contains_key(&key) {
                continue;
            }
            inner.rows.insert(
                key,
                Row {
                    cell: Box::new(VhStaticCell::new()),
                    target_class: 0,
                },
            );
            inner.unresolved.push(key);
        }
        self.unresolved.store(inner.unresolved.len(), Ordering::Relaxed);
    }

    /// The address of `key`'s cell, `0` for none (or a momentarily held
    /// lock: a compile never waits, the site keeps its call).
    pub(crate) fn cell_addr(&self, key: (u32, u32)) -> usize {
        self.inner.try_lock().map_or(0, |inner| {
            inner
                .rows
                .get(&key)
                .map_or(0, |row| &*row.cell as *const VhStaticCell as usize)
        })
    }

    /// Is any registered key still waiting for a note?
    pub(crate) fn has_unresolved(&self) -> bool {
        self.unresolved.load(Ordering::Relaxed) != 0
    }

    /// Settle every unresolved key whose field holds the handle at `vh`
    /// (`handle_of` answers a field's current handle address, `None` while
    /// its class is not initialized): filled with `target`, or left unfilled
    /// for good when `target` is `None`. Returns how many were filled.
    pub(crate) fn settle(
        &self,
        vh: usize,
        target: Option<VhStaticTarget>,
        handle_of: &dyn Fn((u32, u32)) -> Option<usize>,
    ) -> usize {
        let mut inner = self.inner.lock();
        let mut filled = 0;
        let mut i = 0;
        while i < inner.unresolved.len() {
            let key = inner.unresolved[i];
            if handle_of(key) != Some(vh) {
                i += 1;
                continue;
            }
            inner.unresolved.swap_remove(i);
            let (Some(t), Some(row)) = (target, inner.rows.get_mut(&key)) else {
                continue;
            };
            if row.cell.fill(t.base_cell, t.field_index, t.kind) {
                row.target_class = t.class_id;
                filled += 1;
            }
        }
        self.unresolved.store(inner.unresolved.len(), Ordering::Relaxed);
        filled
    }

    /// Clear and retire the cells naming an unloaded class.
    pub(crate) fn forget_classes(&self, unloaded: &dyn Fn(u32) -> bool) {
        let mut inner = self.inner.lock();
        let gone: Vec<(u32, u32)> = inner
            .rows
            .iter()
            .filter(|(key, row)| unloaded(key.0) || (row.target_class != 0 && unloaded(row.target_class)))
            .map(|(key, _)| *key)
            .collect();
        for key in gone {
            if let Some(row) = inner.rows.remove(&key) {
                row.cell.clear();
                inner.retired.push(row.cell);
            }
        }
        let Inner {
            rows, unresolved, ..
        } = &mut *inner;
        unresolved.retain(|key| rows.contains_key(key));
        self.unresolved.store(unresolved.len(), Ordering::Relaxed);
    }
}

/// The natives' report (`NativeSystemAccess::note_static_varhandle_target`):
/// `vh` names `class_id`'s static `field_index`, whose class is initialized.
/// One relaxed load while nothing is unresolved.
pub(crate) fn note_target(
    shared: &SharedVm,
    vh: ObjectRef,
    class_id: ClassId,
    field_index: usize,
    writable: bool,
    desc: &dyn Fn() -> Option<u8>,
) {
    let cells = &shared.jit.vh_static_cells;
    if !cells.has_unresolved() {
        return;
    }
    let target = if writable {
        static_target(shared, vh, class_id, field_index, desc)
    } else {
        None
    };
    let filled = cells.settle(vh.as_ptr() as usize, target, &|(class, index)| {
        field_handle_addr(shared, class, index)
    });
    if filled != 0 && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] vh-const-static filed {filled} cell(s): class_id={} static={field_index} \
             kind={:?}",
            class_id.as_u32(),
            target.map(|t| char::from(t.kind)),
        );
    }
}

/// The target a writable handle's cells are filled with, or `None` (switch
/// off; the natives' static atomic is not the hardware CAS on the payload; a
/// class whose statics the compiled `getstatic` does not read inline; not a
/// whole `int` / `long`; no published block; an invoke-exact handle).
fn static_target(
    shared: &SharedVm,
    vh: ObjectRef,
    class_id: ClassId,
    field_index: usize,
    desc: &dyn Fn() -> Option<u8>,
) -> Option<VhStaticTarget> {
    let raw = class_id.as_u32();
    if !vh_const_static_enabled()
        || !shared.hw_static_cas
        || crate::vm::statics_index_disabled()
        || shared.classes.system_class_id.load(Ordering::Relaxed) == raw
        || shared.classes.boolean_class_id.load(Ordering::Relaxed) == raw
    {
        return None;
    }
    let kind = desc().filter(|k| matches!(k, b'I' | b'J'))?;
    if crate::runtime::interpreter::var_handle_exact_gate(shared)
        .is_some_and(|slot| crate::runtime::interpreter::var_handle_receiver_is_exact(shared, vh, slot))
    {
        return None;
    }
    let base_cell = shared
        .classes
        .statics_index
        .base_cell_addr(class_id, field_index)?;
    Some(VhStaticTarget {
        base_cell,
        field_index,
        kind,
        class_id: raw,
    })
}

/// The current address of the handle the `static final` `(class, index)`
/// holds, `None` while the class is not initialized (its `static final` may
/// not hold its handle yet) or the field holds none.
fn field_handle_addr(shared: &SharedVm, class: u32, index: u32) -> Option<usize> {
    if !shared.classes.class_init_bits.is_set(class) {
        return None;
    }
    // Cast: a u32 static index.
    match crate::vm::get_static_shared(shared, ClassId::new(class), index as usize) {
        Value::Object(Some(handle)) => Some(handle.as_ptr() as usize),
        _ => None,
    }
}

#[cfg(test)]
mod j27_vhcomp_static_cells_tests {
    use super::*;

    const TARGET: VhStaticTarget = VhStaticTarget {
        base_cell: 0x4000,
        field_index: 2,
        kind: b'I',
        class_id: 77,
    };

    fn filed(cells: &VhStaticCells, key: (u32, u32)) -> Option<(usize, u32, u8)> {
        let addr = cells.cell_addr(key);
        assert_ne!(addr, 0, "{key:?} has a cell");
        // SAFETY: a cell address `cell_addr` answered; the table owns it for
        // its life and never frees one.
        unsafe { &*(addr as *const VhStaticCell) }.filed()
    }

    #[test]
    fn a_registered_field_has_an_unfilled_cell_until_its_handle_is_noted() {
        let cells = VhStaticCells::new();
        assert_eq!(cells.cell_addr((5, 0)), 0, "nothing registered");
        cells.register(&[(5, 0), (5, 1)]);
        cells.register(&[(5, 0)]);
        let addr = cells.cell_addr((5, 0));
        assert!(cells.has_unresolved());
        assert_eq!(filed(&cells, (5, 0)), None);
        // Field (5,0) holds handle 0x100, field (5,1) handle 0x200.
        let handle_of = |key: (u32, u32)| -> Option<usize> { Some(if key.1 == 0 { 0x100 } else { 0x200 }) };
        assert_eq!(cells.settle(0x300, Some(TARGET), &handle_of), 0, "another handle");
        assert_eq!(cells.settle(0x100, Some(TARGET), &handle_of), 1);
        assert_eq!(filed(&cells, (5, 0)), Some((0x4000, 32, b'I')));
        assert_eq!(cells.cell_addr((5, 0)), addr, "the address a compile baked");
        assert_eq!(filed(&cells, (5, 1)), None);
        assert!(cells.has_unresolved());
        // A read-only (or otherwise refused) handle settles its field unfilled.
        assert_eq!(cells.settle(0x200, None, &handle_of), 0);
        assert!(!cells.has_unresolved());
        assert_eq!(filed(&cells, (5, 1)), None);
    }

    #[test]
    fn an_uninitialized_holder_stays_unresolved() {
        let cells = VhStaticCells::new();
        cells.register(&[(9, 4)]);
        assert_eq!(cells.settle(0x100, Some(TARGET), &|_| None), 0);
        assert!(cells.has_unresolved());
        assert_eq!(cells.settle(0x100, Some(TARGET), &|_| Some(0x100)), 1);
    }

    #[test]
    fn an_unload_of_either_class_clears_the_cell() {
        let cells = VhStaticCells::new();
        cells.register(&[(5, 0), (6, 0), (8, 0)]);
        let all = |_: (u32, u32)| -> Option<usize> { Some(0x100) };
        assert_eq!(cells.settle(0x100, Some(TARGET), &all), 3);
        let held = cells.cell_addr((6, 0));
        cells.forget_classes(&|id| id == 5);
        assert_eq!(cells.cell_addr((5, 0)), 0, "the handle's holder unloaded");
        assert_eq!(filed(&cells, (6, 0)), Some((0x4000, 32, b'I')));
        cells.forget_classes(&|id| id == TARGET.class_id);
        assert_eq!(cells.cell_addr((6, 0)), 0, "the static's class unloaded");
        // SAFETY: a retired cell stays allocated for compiled code.
        assert_eq!(unsafe { &*(held as *const VhStaticCell) }.filed(), None);
    }
}
