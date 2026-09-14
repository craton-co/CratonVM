// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMS §5.3.4 loader constraints.
//!
//! ## What the spec requires, and what this VM did instead
//!
//! When a class `C` defined by loader `L1` refers to a type `N` that appears in
//! the descriptor of a member it resolves in class `D` defined by loader `L2`,
//! §5.3.4 requires the VM to impose the constraint `N^L1 = N^L2`: both loaders
//! must see the *same* runtime class for that name. If they do not — because
//! each loader defined its own `N` — the resolution must fail with a
//! `LinkageError`.
//!
//! Without that check, `L1` can hand an object of *its* `N` to a method in `D`
//! that was verified against `L2`'s `N`. The verifier passed, because it
//! checked each side against its own namespace. The result is **type
//! confusion**: field offsets and vtable indices read against the wrong layout,
//! with no cast and no error. It is the loader-namespace analogue of the
//! array-class defect this branch already fixed, where `X[]` from two loaders
//! collapsed to one runtime class
//! (`array-class-defining-loader.md`).
//!
//! A workspace-wide grep for "loader constraint" before this module found
//! nothing but comments. §5.3.4 was unimplemented.
//!
//! ## Representation
//!
//! HotSpot's shape, which is the one that makes the check cheap: for each
//! name, the loaders that must agree form an equivalence class, and each class
//! may carry a *pinned* resolution. Union-find over `(name, loader)`:
//!
//! * [`LoaderConstraints::impose`] merges two loaders' sets for one name. If
//!   both sets are already pinned to **different** classes, that is the
//!   violation, and it is reported at the moment of imposition.
//! * [`LoaderConstraints::pin`] records "this loader resolved this name to this
//!   class". If the set is already pinned to a different class, that is the
//!   same violation, discovered from the other direction.
//!
//! Both directions are needed because a constraint can be imposed before or
//! after either side actually loads the name — resolution order is not fixed.
//!
//! ## Fail-closed, but not fail-loud yet
//!
//! Detection is separated from enforcement on purpose. A false `LinkageError`
//! breaks an application that works today, and this VM has no measurement of
//! how often real code trips a constraint. So the table **records** violations
//! and counts them; turning a violation into a thrown `LinkageError` is a
//! separate, flagged step, in the same "measurement, not deletion" shape the
//! `--jdk-only` policy already uses.
//!
//! What is NOT deferred: the check itself is exercised at a real call site, so
//! this is not a mechanism nobody drives. See `ClassManager`'s supertype link
//! path.

use std::collections::HashMap;

/// A loader id as this crate spells it — the same `u32` namespace
/// `ClassLoaderId` uses, flattened, so this module has no dependency on the
/// loader enum's shape.
pub type LoaderKey = u32;

/// A class id as this crate spells it, flattened for the same reason.
pub type ClassKey = u32;

/// Two loaders that were required to agree on a name resolved it differently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoaderConstraintViolation {
    /// The type name both loaders were required to agree on.
    pub name: String,
    /// The class one side had already resolved the name to.
    pub existing: ClassKey,
    /// The class the other side resolved it to.
    pub conflicting: ClassKey,
}

impl LoaderConstraintViolation {
    /// The message a `LinkageError` would carry, once enforcement is enabled.
    /// Spelled here so the recording path and the future throwing path cannot
    /// describe the same violation differently.
    pub fn message(&self) -> String {
        format!(
            "loader constraint violation: type {} is resolved to two different \
             classes ({} and {}) by loaders that JVMS 5.3.4 requires to agree",
            self.name, self.existing, self.conflicting
        )
    }
}

/// One equivalence class of loaders for one name.
#[derive(Clone, Debug, Default)]
struct Group {
    /// Union-find parent index within `groups`.
    parent: usize,
    /// The class this group has been pinned to, if any loader in it has
    /// resolved the name yet.
    pinned: Option<ClassKey>,
}

/// The §5.3.4 constraint table.
#[derive(Debug, Default)]
pub struct LoaderConstraints {
    /// `(name, loader) -> group index`.
    index: HashMap<(String, LoaderKey), usize>,
    groups: Vec<Group>,
    /// Every violation observed, in order. Bounded by `MAX_RECORDED`.
    violations: Vec<LoaderConstraintViolation>,
    /// Total violations observed, including any past `MAX_RECORDED`.
    violation_count: u64,
}

/// Cap on retained violation records. The counter is exact regardless — an
/// unbounded `Vec` here would turn a pathological app into an OOM, and this is
/// a diagnostic.
const MAX_RECORDED: usize = 256;

impl LoaderConstraints {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.groups[i].parent != i {
            let grandparent = self.groups[self.groups[i].parent].parent;
            self.groups[i].parent = grandparent;
            i = grandparent;
        }
        i
    }

    fn group_for(&mut self, name: &str, loader: LoaderKey) -> usize {
        if let Some(&i) = self.index.get(&(name.to_string(), loader)) {
            return self.find(i);
        }
        let i = self.groups.len();
        self.groups.push(Group {
            parent: i,
            pinned: None,
        });
        self.index.insert((name.to_string(), loader), i);
        i
    }

    fn record(&mut self, v: LoaderConstraintViolation) {
        self.violation_count += 1;
        if self.violations.len() < MAX_RECORDED {
            self.violations.push(v);
        }
    }

    /// Impose `name^a = name^b` (JVMS §5.3.4).
    ///
    /// Returns the violation if the two sides are already pinned to different
    /// classes. The merge still happens: the table stays a faithful record of
    /// what was *required*, so a later `pin` cannot silently succeed against a
    /// group that is already known to be inconsistent.
    pub fn impose(
        &mut self,
        name: &str,
        a: LoaderKey,
        b: LoaderKey,
    ) -> Option<LoaderConstraintViolation> {
        let ga = self.group_for(name, a);
        let gb = self.group_for(name, b);
        if ga == gb {
            return None;
        }
        let pa = self.groups[ga].pinned;
        let pb = self.groups[gb].pinned;
        let violation = match (pa, pb) {
            (Some(x), Some(y)) if x != y => Some(LoaderConstraintViolation {
                name: name.to_string(),
                existing: x,
                conflicting: y,
            }),
            _ => None,
        };
        // Merge b into a, keeping whichever pin exists. On a violation the
        // surviving pin is `a`'s — arbitrary, and it does not matter, because
        // the group is recorded as violated either way.
        self.groups[gb].parent = ga;
        if self.groups[ga].pinned.is_none() {
            self.groups[ga].pinned = pb;
        }
        if let Some(v) = violation.clone() {
            self.record(v);
        }
        violation
    }

    /// Record that `loader` resolved `name` to `class`.
    ///
    /// Returns the violation if this group was already pinned to a different
    /// class — i.e. some other loader that was required to agree resolved the
    /// same name elsewhere.
    pub fn pin(
        &mut self,
        name: &str,
        loader: LoaderKey,
        class: ClassKey,
    ) -> Option<LoaderConstraintViolation> {
        let g = self.group_for(name, loader);
        match self.groups[g].pinned {
            Some(existing) if existing != class => {
                let v = LoaderConstraintViolation {
                    name: name.to_string(),
                    existing,
                    conflicting: class,
                };
                self.record(v.clone());
                Some(v)
            }
            Some(_) => None,
            None => {
                self.groups[g].pinned = Some(class);
                None
            }
        }
    }

    /// What this loader must resolve `name` to, if the constraint set already
    /// determines it. `None` means unconstrained, not "no such class".
    pub fn required_class(&mut self, name: &str, loader: LoaderKey) -> Option<ClassKey> {
        let key = (name.to_string(), loader);
        let i = *self.index.get(&key)?;
        let g = self.find(i);
        self.groups[g].pinned
    }

    /// Total violations observed, including any beyond the retained cap.
    pub fn violation_count(&self) -> u64 {
        self.violation_count
    }

    /// The retained violations, oldest first.
    pub fn violations(&self) -> &[LoaderConstraintViolation] {
        &self.violations
    }

    /// Number of distinct `(name, loader)` pairs the table has seen.
    pub fn tracked_pairs(&self) -> usize {
        self.index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agreeing_loaders_are_not_a_violation() {
        let mut c = LoaderConstraints::new();
        assert!(c.pin("Foo", 1, 100).is_none());
        assert!(c.pin("Foo", 2, 100).is_none());
        assert!(c.impose("Foo", 1, 2).is_none());
        assert_eq!(c.violation_count(), 0);
    }

    #[test]
    fn two_loaders_with_their_own_foo_violate_an_imposed_constraint() {
        let mut c = LoaderConstraints::new();
        // Each loader legitimately defines its own Foo. That alone is legal —
        // distinct namespaces — and must NOT be a violation on its own.
        assert!(c.pin("Foo", 1, 100).is_none());
        assert!(c.pin("Foo", 2, 200).is_none());
        assert_eq!(c.violation_count(), 0, "two namespaces alone are legal");

        // It becomes a violation only once something requires them to agree.
        let v = c.impose("Foo", 1, 2).expect("must violate");
        assert_eq!(v.name, "Foo");
        assert_eq!((v.existing, v.conflicting), (100, 200));
        assert_eq!(c.violation_count(), 1);
        assert!(v.message().contains("5.3.4"));
    }

    #[test]
    fn a_constraint_imposed_before_either_side_loads_is_caught_on_pin() {
        // Resolution order is not fixed, so the check has to work from both
        // directions. This is the direction `impose` alone would miss.
        let mut c = LoaderConstraints::new();
        assert!(c.impose("Bar", 1, 2).is_none(), "nothing pinned yet");
        assert!(c.pin("Bar", 1, 100).is_none());
        let v = c
            .pin("Bar", 2, 200)
            .expect("must violate on the second pin");
        assert_eq!((v.existing, v.conflicting), (100, 200));
    }

    #[test]
    fn constraints_are_transitive() {
        let mut c = LoaderConstraints::new();
        c.impose("Baz", 1, 2);
        c.impose("Baz", 2, 3);
        assert!(c.pin("Baz", 1, 100).is_none());
        // Loader 3 was never named alongside 1 directly.
        let v = c
            .pin("Baz", 3, 300)
            .expect("transitive constraint must bind");
        assert_eq!((v.existing, v.conflicting), (100, 300));
    }

    #[test]
    fn names_do_not_bleed_into_each_other() {
        let mut c = LoaderConstraints::new();
        c.impose("A", 1, 2);
        assert!(c.pin("A", 1, 100).is_none());
        // A constraint on A says nothing about B.
        assert!(c.pin("B", 2, 999).is_none());
        assert_eq!(c.violation_count(), 0);
    }

    #[test]
    fn required_class_answers_only_when_determined() {
        let mut c = LoaderConstraints::new();
        assert_eq!(c.required_class("Q", 1), None, "unconstrained");
        c.impose("Q", 1, 2);
        c.pin("Q", 2, 42);
        assert_eq!(
            c.required_class("Q", 1),
            Some(42),
            "loader 1 is bound by loader 2's resolution"
        );
    }

    #[test]
    fn the_retained_violation_list_is_bounded_but_the_count_is_not() {
        let mut c = LoaderConstraints::new();
        for i in 0..(MAX_RECORDED as u32 + 10) {
            let name = format!("N{i}");
            c.pin(&name, 1, 1);
            c.pin(&name, 2, 2);
            c.impose(&name, 1, 2);
        }
        assert_eq!(c.violations().len(), MAX_RECORDED);
        assert_eq!(c.violation_count(), MAX_RECORDED as u64 + 10);
    }
}
