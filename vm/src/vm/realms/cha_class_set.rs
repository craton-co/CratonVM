// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane PROXY; page
//! `docs/known-issues/jit/proxy-row-residual-is-classfile-api-code-quality-and-flat-vm-dispatch-cost-20261024.md`;
//! `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`, opt-in): the EXACT
//! classes an `invokeinterface` selects one inherited body for.
//!
//! The optimizing tier's class-set guard (`cratonvm_jit::ir` `cha_class_set`)
//! splices a body behind `word == c0 | word == c1 | ...`. Each `c` must be a
//! class for which the JVMS 5.4.6 selection of the site's method, on an
//! object of exactly `c`, is the spliced body. This module answers that list,
//! class by class, from the store as it stands:
//!
//! * `c` is a concrete class that implements the site's interface (else the
//!   interpreter would throw `IncompatibleClassChangeError` there, which the
//!   miss edge's call still does);
//! * `c`'s superclass chain reaches the body's declaring class `D` with no
//!   class on the way declaring the method at all (any declaration there,
//!   abstract, private or static, refuses `c`: selection could stop on it);
//! * `D`'s declaration is `public`, not `static`, not `abstract` (a
//!   non-public selected method throws `IllegalAccessError`);
//! * no class on the chain is a compatibility stub, whose body can be
//!   upgraded in place under the same id.
//!
//! No class-hierarchy ASSUMPTION is made, so nothing needs invalidating: class
//! ids are never reused, a class defined later is simply not in the list (it
//! takes the call), and a redefinition cannot change a class's method set.
//! The analysis does not call the metadata-table resolvers
//! (`runtime::resolve::guard`): it reads one class's own declarations at a
//! time, as the watermark survey's stub check does.

use super::class_realm::{ClassRealm, CHA_MAX_SUBTYPES, LAMBDA_PROXY_ID_BASE};
use crate::classloading::{Class, ClassId, ClassStore};
use cratonvm_reader::class_access_flags::MethodAccessFlags;

/// The most classes one answer lists (`cratonvm_jit::ir::CHA_CLASS_SET_MAX`,
/// the builder's own cut). Any subset is sound.
pub const CHA_CLASS_SET_MAX: usize = cratonvm_jit::ir::CHA_CLASS_SET_MAX;

/// Ceiling on one superclass-chain walk (a malformed cyclic chain ends here).
const CHA_CLASS_SET_MAX_DEPTH: usize = 64;

impl ClassRealm {
    /// The loaded classes for which an `invokeinterface
    /// site_interface.name descriptor` on an object of EXACTLY that class
    /// selects `declaring`'s own declaration, `first` (the receiver profile's
    /// class) first, at most [`CHA_CLASS_SET_MAX`]. Empty when `first` is not
    /// one of them, when `site_interface` is not an interface, when
    /// `declaring`'s declaration cannot be an interface call's target, and
    /// when `declaring` has more than [`CHA_MAX_SUBTYPES`] subtypes.
    pub fn cha_interface_class_set(
        &self,
        site_interface: ClassId,
        declaring: ClassId,
        first: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Vec<u32> {
        let cm = self.class_manager.read();
        let query = ClassSetQuery {
            store: cm.class_store(),
            site_interface,
            declaring,
            name,
            descriptor,
        };
        query.class_set(first)
    }
}

/// One class-set question against one store snapshot.
struct ClassSetQuery<'a> {
    store: &'a ClassStore,
    site_interface: ClassId,
    declaring: ClassId,
    name: &'a str,
    descriptor: &'a str,
}

impl ClassSetQuery<'_> {
    /// The answer of [`ClassRealm::cha_interface_class_set`].
    fn class_set(&self, first: ClassId) -> Vec<u32> {
        if !self.admits_body() {
            return Vec::new();
        }
        let Some(subtypes) = self.store.subtypes_of(self.declaring, CHA_MAX_SUBTYPES) else {
            return Vec::new();
        };
        let mut set: Vec<u32> = Vec::new();
        for cid in [first, self.declaring].into_iter().chain(subtypes) {
            if set.len() >= CHA_CLASS_SET_MAX {
                break;
            }
            if !set.contains(&cid.as_u32()) && self.selects_body_for(cid) {
                set.push(cid.as_u32());
            }
        }
        if set.first() != Some(&first.as_u32()) {
            set.clear();
        }
        set
    }

    /// The site names an interface, and `declaring` is a class (no stub)
    /// whose own declaration can be an interface call's selected method.
    fn admits_body(&self) -> bool {
        let interface_site = self
            .store
            .get(self.site_interface)
            .is_some_and(|c| c.is_interface());
        let Some(class) = self.store.get(self.declaring) else {
            return false;
        };
        interface_site
            && !class.is_interface()
            && !is_compatibility_stub(class)
            && class
                .find_method(self.name, self.descriptor)
                .is_some_and(|m| interface_selectable(m.access_flags))
    }

    /// Does an `invokeinterface` on an object of exactly `cid` select
    /// `declaring`'s body?
    fn selects_body_for(&self, cid: ClassId) -> bool {
        let id = cid.as_u32();
        if id == 0 || id >= LAMBDA_PROXY_ID_BASE {
            return false;
        }
        let Some(class) = self.store.get(cid) else {
            return false;
        };
        let concrete =
            !class.is_interface() && !class.is_abstract() && !class.name.starts_with('[');
        concrete
            && class.is_subclass_of(self.site_interface, self.store)
            && self.chain_reaches_declaring(cid)
    }

    /// `cid`'s superclass chain reaches `declaring`, and no class before it
    /// declares the method or is a compatibility stub.
    fn chain_reaches_declaring(&self, cid: ClassId) -> bool {
        let mut current = cid;
        for _ in 0..CHA_CLASS_SET_MAX_DEPTH {
            if current == self.declaring {
                return true;
            }
            let Some(class) = self.store.get(current) else {
                return false;
            };
            if is_compatibility_stub(class)
                || class.find_method(self.name, self.descriptor).is_some()
            {
                return false;
            }
            match class.superclass {
                Some(up) => current = up,
                None => return false,
            }
        }
        false
    }
}

/// A declaration an interface call may select and run: `public`, an
/// instance method, with a body.
fn interface_selectable(flags: MethodAccessFlags) -> bool {
    flags.contains(MethodAccessFlags::PUBLIC)
        && !flags.intersects(MethodAccessFlags::STATIC | MethodAccessFlags::ABSTRACT)
}

fn is_compatibility_stub(class: &Class) -> bool {
    matches!(
        class.origin,
        cratonvm_classloading::ClassOrigin::CompatibilityStub { .. }
    )
}
