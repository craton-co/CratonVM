// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A per-VM, descriptor-keyed table of `java.lang.invoke.MethodType` objects
//! for the types `MethodHandle.bindTo` gives the handles it mints (JIT round
//! 16, lane mh; proposal MH3-2 of `jit-r15w4-mh3-proposals-RETIRED-20261003.md`).
//!
//! # Why
//!
//! Every `bindTo` built a fresh `MethodType` for the bound handle's `type`: a
//! `Class[]`, the `MethodType`, its fabricated `MethodTypeForm` and the form's
//! two cache arrays (16 and 64 references), about a kilobyte and five
//! allocations per bind. HotSpot interns the type (`MethodType.internTable`),
//! so `mh.bindTo(a).type() == mh.bindTo(b).type()` there (measured,
//! `R16MhFloor`), and it was `false` here. A bound handle that carries a fresh
//! `type` also misses the direct lane's per-handle record memo, which keys on
//! the addresses of the handle's four field objects (`MH_CLASS` / `MH_NAME` /
//! `MH_DESC` are already shared with the target since round 15 wave 4).
//!
//! # Ownership (per-VM, NOT process-global)
//!
//! One table is a field of each VM's class realm, reached through
//! `NativeContext::bound_method_type_get` / `bound_method_type_put`, and
//! scanned and remapped through that VM's `memory::native_roots` row
//! (`bound-method-types`), exactly like the `ObjectStreamClass` cache. There
//! is no static: another VM's collection can neither see nor rewrite a row,
//! and the rows go with the VM.
//!
//! # What may be stored
//!
//! The CALLER decides (`lang_invoke::bound_method_type_shareable`): only a
//! descriptor whose every reference component is a loaded `java/...` class or
//! a primitive, so a row's mirrors belong to the bootstrap / platform loaders,
//! never unload, and the row is rooted outright without retaining a user
//! loader. The table is bounded and refuses inserts past
//! [`MethodTypeTable::CAPACITY`] (the caller then keeps its fresh object).
//!
//! # Class-keyed rows (round 16 wave 15, lane mhapp)
//!
//! A descriptor alone names a class only by NAME, and two loaders may each
//! define a class of one name. A `java/...` name is safe (only the built-in
//! loaders may define `java.*`), but `(LBox;)I` is not: a row stored for the
//! application loader's `Box` would also answer a bind whose `Box` a user
//! loader defined. Under `CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES` a type
//! naming any class outside `java/` is therefore stored under
//! [`class_keyed`]: the descriptor plus the `ClassId` the build resolved for
//! every reference component, in order. Another loader's `Box` is another
//! `ClassId`, so another row. A row naming a class that unloads is dropped by
//! [`MethodTypeTable::forget_classes`] (the VM's class-unload transaction),
//! so a `ClassId` a later class reuses can never be served the dead class's
//! row. A redefined class keeps its `ClassId` and its mirror, as HotSpot
//! keeps the `Class` a `MethodType` names, so its row stays right.
//!
//! # Signature-polymorphic linker artifacts (i61)
//!
//! A key whose descriptor starts with `linker:` stores an `Object[2]` holding
//! the JDK linker's MemberName and MethodType appendix. Such a prefix cannot
//! occur in a method descriptor, so these rows cannot collide with bound types.
//! Only sites whose types pass the bootstrap/platform shareability predicate
//! are cached, so their appendices retain no user loader. Their class-key
//! suffix names the caller class, whose loader resolves the site's types. They use the same per-VM root scan, relocation, capacity and
//! unload transaction; no process-global cache or unrooted reference is used.
//!
//! A key starting with `linker-basic:` (interpreter round i4 wave 95, lane L4,
//! `CRATONVM_MH_LINKER_BASIC_MEMBER`) stores the JDK linker's `MemberName`
//! ALONE, keyed by the linker name, the call site's basic type and the caller
//! class: `Invokers.methodHandleInvokeLinkerMethod`'s member is a function of
//! the basic type, a JDK form's entry naming no application class, so the row
//! retains no user loader even for a site whose own types do.
//!
//! # Lock discipline
//!
//! The lock guards plain map operations only: no allocation of heap objects,
//! no `ctx` call and no Java runs under it. GC root scans take it at a
//! safepoint.

use cratonvm_types::ObjectRef;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;

/// Separates a class-keyed row's descriptor from its class ids. A method
/// descriptor never holds a NUL, so a class-keyed row never equals a
/// descriptor-only one.
const CLASS_KEY_SEPARATOR: char = '\0';

/// The row key of a bound type naming classes outside `java/` (see the module
/// docs): `desc`, then the raw `ClassId` the build resolved for each reference
/// component, in descriptor order.
pub fn class_keyed(desc: &str, class_ids: &[u32]) -> String {
    use std::fmt::Write as _;
    let mut key = String::with_capacity(desc.len() + 1 + class_ids.len() * 6);
    key.push_str(desc);
    key.push(CLASS_KEY_SEPARATOR);
    for (i, id) in class_ids.iter().enumerate() {
        if i > 0 {
            key.push(',');
        }
        let _ = write!(key, "{id}");
    }
    key
}

/// The class ids a [`class_keyed`] row names; none for a descriptor-only row.
fn key_class_ids(key: &str) -> impl Iterator<Item = u32> + '_ {
    key.split_once(CLASS_KEY_SEPARATOR)
        .map(|(_, ids)| ids)
        .unwrap_or("")
        .split(',')
        .filter_map(|id| id.parse().ok())
}

/// See the module docs.
#[derive(Default)]
pub struct MethodTypeTable {
    inner: RwLock<FxHashMap<Box<str>, ObjectRef>>,
}

impl MethodTypeTable {
    /// The most rows one VM keeps. Bound types are a small, program-shaped
    /// set (one per distinct bound descriptor); past this the table stops
    /// growing and the caller keeps its fresh object, which is the
    /// pre-table behaviour.
    pub const CAPACITY: usize = 4096;

    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// The `MethodType` stored for `desc`, if any.
    pub fn get(&self, desc: &str) -> Option<ObjectRef> {
        self.inner.read().get(desc).copied()
    }

    /// Store `mt` for `desc` unless a row exists, and answer the row's object
    /// (the earlier one when two threads raced, so both see one object), or
    /// `mt` itself when the table is full.
    pub fn insert_if_absent(&self, desc: &str, mt: ObjectRef) -> ObjectRef {
        let mut map = self.inner.write();
        if let Some(&existing) = map.get(desc) {
            return existing;
        }
        if map.len() >= Self::CAPACITY {
            return mt;
        }
        map.insert(Box::from(desc), mt);
        mt
    }

    /// Hand `visit` every stored object (the VM roots each one).
    pub fn scan_roots(&self, visit: &mut dyn FnMut(ObjectRef)) {
        for mt in self.inner.read().values() {
            visit(*mt);
        }
    }

    /// Rewrite stored objects a moving collection relocated. A forwarding
    /// entry of `0` (the stale-entry case the lambda call-site cache's `C20`
    /// guard handles) drops the row rather than storing a null.
    pub fn remap_roots(&self, pointer_map: &cratonvm_types::PointerMap) {
        if pointer_map.is_empty() {
            return;
        }
        self.inner
            .write()
            .retain(|_, mt| match pointer_map.get(&(mt.as_ptr() as usize)) {
                Some(&0) => false,
                Some(&new) => {
                    // SAFETY: a collector forwarding map holds live, aligned
                    // object starts for every non-zero target.
                    *mt = unsafe { ObjectRef::from_raw(new as *mut u8) };
                    true
                }
                None => true,
            });
    }

    /// Drop every [`class_keyed`] row that names a class `gone` answers true
    /// for (the VM's class-unload transaction passes the unloaded ids), and
    /// answer how many went. Descriptor-only rows name `java/...` classes,
    /// which never unload, and are kept.
    pub fn forget_classes(&self, gone: &dyn Fn(u32) -> bool) -> usize {
        let mut map = self.inner.write();
        let before = map.len();
        map.retain(|key, _| !key_class_ids(key).any(gone));
        before - map.len()
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Whether the table has no rows.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_ref(n: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the table only stores and copies it.
        unsafe { ObjectRef::from_raw((n * 8 + 8) as *mut u8) }
    }

    #[test]
    fn one_descriptor_answers_one_object() {
        let t = MethodTypeTable::new();
        assert!(t.get("(I)I").is_none());
        let first = t.insert_if_absent("(I)I", fake_ref(1));
        let second = t.insert_if_absent("(I)I", fake_ref(2));
        assert_eq!(first, fake_ref(1));
        assert_eq!(second, fake_ref(1), "a later build answers the first row");
        assert_eq!(t.get("(I)I"), Some(fake_ref(1)));
        assert_eq!(t.insert_if_absent("(J)I", fake_ref(3)), fake_ref(3));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn a_full_table_answers_the_callers_object_and_stores_nothing() {
        let t = MethodTypeTable::new();
        for i in 0..MethodTypeTable::CAPACITY {
            t.insert_if_absent(&format!("(I)L{i};"), fake_ref(i + 1));
        }
        let extra = fake_ref(MethodTypeTable::CAPACITY + 10);
        assert_eq!(t.insert_if_absent("()V", extra), extra);
        assert!(t.get("()V").is_none());
        assert_eq!(t.len(), MethodTypeTable::CAPACITY);
    }

    #[test]
    fn scan_and_remap_cover_only_this_table() {
        let a = MethodTypeTable::new();
        let b = MethodTypeTable::new();
        a.insert_if_absent("(I)I", fake_ref(1));
        b.insert_if_absent("(I)I", fake_ref(1));
        let mut roots = Vec::new();
        a.scan_roots(&mut |r| roots.push(r));
        assert_eq!(roots, vec![fake_ref(1)]);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(fake_ref(1).as_ptr() as usize, fake_ref(9).as_ptr() as usize);
        b.remap_roots(&map);
        assert_eq!(b.get("(I)I"), Some(fake_ref(9)), "b's own row moves");
        assert_eq!(a.get("(I)I"), Some(fake_ref(1)), "a is untouched");
    }

    /// Round 16 wave 15 (mhapp): one descriptor naming two loaders' classes of
    /// one name is two rows, and neither is the descriptor-only row.
    #[test]
    fn class_keyed_rows_of_one_descriptor_are_distinct() {
        let t = MethodTypeTable::new();
        let app = class_keyed("(LBox;)I", &[40]);
        let user = class_keyed("(LBox;)I", &[41]);
        assert_ne!(app, user);
        assert_ne!(app, "(LBox;)I");
        assert_eq!(
            class_keyed("(LA;LB;)LC;", &[1, 22, 333]),
            "(LA;LB;)LC;\u{0}1,22,333"
        );
        assert_eq!(t.insert_if_absent(&app, fake_ref(1)), fake_ref(1));
        assert_eq!(t.insert_if_absent(&user, fake_ref(2)), fake_ref(2));
        assert!(t.get("(LBox;)I").is_none(), "no descriptor-only row");
        assert_eq!(t.get(&app), Some(fake_ref(1)));
        assert_eq!(t.get(&user), Some(fake_ref(2)));
    }

    /// An unloaded class's rows go; descriptor-only rows and rows naming
    /// other classes stay.
    #[test]
    fn forget_classes_drops_only_rows_naming_a_gone_class() {
        let t = MethodTypeTable::new();
        t.insert_if_absent("(I)I", fake_ref(1));
        t.insert_if_absent(&class_keyed("(LBox;)I", &[40]), fake_ref(2));
        t.insert_if_absent(&class_keyed("(LBox;LOther;)I", &[41, 7]), fake_ref(3));
        t.insert_if_absent(&class_keyed("(LOther;)V", &[7]), fake_ref(4));
        assert_eq!(t.forget_classes(&|id| id == 41), 1);
        assert_eq!(t.len(), 3);
        assert_eq!(t.forget_classes(&|id| id == 999), 0);
        assert_eq!(t.forget_classes(&|id| id == 7 || id == 40), 2);
        assert_eq!(
            t.get("(I)I"),
            Some(fake_ref(1)),
            "descriptor-only rows stay"
        );
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn a_zero_forwarding_drops_the_row() {
        let t = MethodTypeTable::new();
        t.insert_if_absent("(I)I", fake_ref(1));
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(fake_ref(1).as_ptr() as usize, 0);
        t.remap_roots(&map);
        assert!(t.get("(I)I").is_none());
        assert!(t.is_empty());
    }
}
