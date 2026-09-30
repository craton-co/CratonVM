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
//! ## Who records, who checks, who throws (interpreter round i1 wave 37, lane L5)
//!
//! * **Recording.** `--jdk-only` member resolution
//!   (`vm/src/runtime/resolve/loader_constraints.rs`, called on a resolution
//!   miss of a field or method whose declaring class has another loader) calls
//!   [`LoaderConstraints::impose`] for every descriptor name one side (or
//!   neither) has loaded yet, and [`LoaderConstraints::pin`]s each side that
//!   has. Two sides that both already see a class are compared directly there
//!   and never reach the table (wave 31). `--compatible` records nothing.
//! * **Checking.** A class definition ([`LoaderConstraints::pinned_for`] before,
//!   refused for a user-defined loader only;
//!   [`LoaderConstraints::pin_if_constrained`] after; `ClassManager`'s define
//!   path) and a VM-initiated load that a loader answered with another loader's
//!   class (the initiating-loader record) consult the table, HotSpot's
//!   `SystemDictionary::check_constraints`. Both are one `is_empty` test while
//!   nothing was ever recorded.
//! * **Throwing.** `--jdk-only` only, with HotSpot's message, which needs the
//!   loaders' `nameAndId` text: the recorder stores it per loader
//!   ([`LoaderConstraints::set_label`]) because the define path holds the
//!   class-manager write lock and cannot read Java objects.
//!
//! Until wave 37 the table's only producer was the superclass link of every
//! define, which is not a §5.3.4 constraint (it is HotSpot's initiating-loader
//! record, which CratonVM keeps elsewhere) and was never read; it is gone, so
//! a define-time check cannot fire on a superclass row the loader-blind
//! supertype fallbacks wrote.

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

/// The HotSpot `ClassLoaderData::loader_name_and_id()` text of one loader,
/// and of its parent when HotSpot prints one (`class_in_module_of_loader`
/// with the parent clause: user-defined loaders only; a null parent is
/// `'bootstrap'`). Measured on JDK 25: `'b' @6d06d69c`, `'app'`,
/// `L5W37X$ChildFirst @1b6d3586` for an unnamed loader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoaderLabel {
    /// `'name' @hash`, `Class @hash`, or `'app'` / `'platform'` / `'bootstrap'`.
    pub name_and_id: String,
    /// The parent loader's `name_and_id`, for a user-defined loader.
    pub parent: Option<String>,
}

/// The §5.3.4 constraint table.
#[derive(Debug, Default)]
pub struct LoaderConstraints {
    /// `name -> [(loader, group index)]`. Keyed by the name alone so a lookup
    /// from a `&str` (the define path's check) allocates nothing; the loaders
    /// that must agree on one name are few.
    index: HashMap<String, Vec<(LoaderKey, usize)>>,
    /// Number of `(name, loader)` rows in `index`.
    pairs: usize,
    groups: Vec<Group>,
    /// Every violation observed, in order. Bounded by `MAX_RECORDED`.
    violations: Vec<LoaderConstraintViolation>,
    /// Total violations observed, including any past `MAX_RECORDED`.
    violation_count: u64,
    /// The message text of each loader a recorder named (see the module doc).
    labels: HashMap<LoaderKey, LoaderLabel>,
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

    /// Nothing was ever recorded (or everything recorded was forgotten): the
    /// define path's whole cost while no constraint exists.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pairs == 0
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.groups[i].parent != i {
            let grandparent = self.groups[self.groups[i].parent].parent;
            self.groups[i].parent = grandparent;
            i = grandparent;
        }
        i
    }

    /// The row of `(name, loader)`, if one exists. Allocation-free.
    fn row(&self, name: &str, loader: LoaderKey) -> Option<usize> {
        self.index
            .get(name)?
            .iter()
            .find(|(l, _)| *l == loader)
            .map(|&(_, i)| i)
    }

    fn group_for(&mut self, name: &str, loader: LoaderKey) -> usize {
        if let Some(i) = self.row(name, loader) {
            return self.find(i);
        }
        let i = self.groups.len();
        self.groups.push(Group {
            parent: i,
            pinned: None,
        });
        self.index
            .entry(name.to_string())
            .or_default()
            .push((loader, i));
        self.pairs += 1;
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
        let i = self.row(name, loader)?;
        let g = self.find(i);
        self.groups[g].pinned
    }

    /// [`Self::required_class`] under the name the define path asks with:
    /// the class a constraint already pins `(name, loader)` to. A definition
    /// of `name` by `loader` is a NEW class, so any answer here is the
    /// violation HotSpot's `check_constraints` refuses. One `is_empty` test
    /// while the table is empty, then one allocation-free hash probe.
    #[inline]
    pub fn pinned_for(&mut self, name: &str, loader: LoaderKey) -> Option<ClassKey> {
        if self.is_empty() {
            return None;
        }
        self.required_class(name, loader)
    }

    /// Record that `loader` now resolves `name` to `class` (it defined it, or
    /// initiated its load), when — and only when — a constraint names
    /// `(name, loader)`: the table never grows a row for an unconstrained
    /// load. The violation when the group is already pinned elsewhere
    /// (counted; the group keeps its pin, as HotSpot's `check_or_update`
    /// leaves the constraint's class in place on a failed check).
    pub fn pin_if_constrained(
        &mut self,
        name: &str,
        loader: LoaderKey,
        class: ClassKey,
    ) -> Option<LoaderConstraintViolation> {
        if self.is_empty() || self.row(name, loader).is_none() {
            return None;
        }
        self.pin(name, loader, class)
    }

    /// Count a definition refused because `(name, loader)` is pinned to
    /// `existing` ([`Self::pinned_for`]); the refused class never existed, so
    /// the record's `conflicting` is `ClassKey::MAX`.
    pub fn note_define_refusal(&mut self, name: &str, existing: ClassKey) {
        self.record(LoaderConstraintViolation {
            name: name.to_string(),
            existing,
            conflicting: ClassKey::MAX,
        });
    }

    /// Store the message text of `loader` (see [`LoaderLabel`]).
    pub fn set_label(&mut self, loader: LoaderKey, label: LoaderLabel) {
        self.labels.insert(loader, label);
    }

    /// The stored message text of `loader`.
    pub fn label(&self, loader: LoaderKey) -> Option<&LoaderLabel> {
        self.labels.get(&loader)
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
        self.pairs
    }

    /// Forget what class unloading made unreachable: every `(name, loader)`
    /// row of a loader in `dead_loader`, and every pin that names a class in
    /// `dead_class`. `groups` is compacted, so the table shrinks. A group
    /// that loses its last row disappears, and a surviving group keeps its
    /// members and loses only a pin that names a tombstone (gc-common w9-e,
    /// `docs/internal/gc-common-round-20260923/common-w9e-loader-constraint-rows-outlive-their-unloaded-loaders-FIXED-20260923.md`).
    /// `violations` and `violation_count` are history and stay as they are.
    ///
    /// Returns the number of index rows dropped.
    pub fn forget(
        &mut self,
        dead_loader: &dyn Fn(LoaderKey) -> bool,
        dead_class: &dyn Fn(ClassKey) -> bool,
    ) -> usize {
        let before = self.pairs;
        for rows in self.index.values_mut() {
            rows.retain(|(loader, _)| !dead_loader(*loader));
        }
        self.index.retain(|_, rows| !rows.is_empty());
        self.labels.retain(|loader, _| !dead_loader(*loader));
        // Roots without path compression, so `groups` stays borrowed shared.
        fn root_of(groups: &[Group], mut i: usize) -> usize {
            while groups[i].parent != i {
                i = groups[i].parent;
            }
            i
        }
        let groups = std::mem::take(&mut self.groups);
        let mut renumber: HashMap<usize, usize> = HashMap::new();
        let mut compacted: Vec<Group> = Vec::new();
        let mut pairs = 0usize;
        for rows in self.index.values_mut() {
            for (_, slot) in rows.iter_mut() {
                let root = root_of(&groups, *slot);
                let new = *renumber.entry(root).or_insert_with(|| {
                    let i = compacted.len();
                    let pinned = groups[root].pinned.filter(|c| !dead_class(*c));
                    compacted.push(Group { parent: i, pinned });
                    i
                });
                *slot = new;
                pairs += 1;
            }
        }
        self.groups = compacted;
        self.pairs = pairs;
        before - self.pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `forget` drops exactly a dead loader's rows and the pins naming dead
    /// classes; survivors keep their groups and answers (gc-common w9-e).
    #[test]
    fn forget_drops_a_dead_loaders_rows_and_dead_pins_only() {
        let mut c = LoaderConstraints::new();
        assert!(c.pin("Foo", 1, 100).is_none());
        assert!(c.impose("Foo", 1, 2).is_none());
        assert!(c.impose("Foo", 2, 3).is_none());
        assert!(c.pin("Bar", 3, 300).is_none());
        let pairs = c.tracked_pairs();
        assert_eq!(pairs, 4, "(Foo,1) (Foo,2) (Foo,3) (Bar,3)");
        // Loader 1 dies; nothing it pinned is unloaded yet.
        assert_eq!(c.forget(&|l| l == 1, &|_| false), 1);
        assert_eq!(c.tracked_pairs(), pairs - 1);
        assert_eq!(c.required_class("Foo", 2), Some(100), "the group keeps its pin");
        assert_eq!(c.required_class("Foo", 3), Some(100));
        assert_eq!(c.required_class("Bar", 3), Some(300));
        // Class 100 is unloaded: the surviving group loses only that pin, and
        // a new class id pinned there is not a violation.
        assert_eq!(c.forget(&|_| false, &|k| k == 100), 0);
        assert_eq!(c.required_class("Foo", 2), None);
        let before = c.violation_count();
        assert!(c.pin("Foo", 2, 101).is_none());
        assert_eq!(c.violation_count(), before);
        assert_eq!(c.required_class("Foo", 3), Some(101), "2 and 3 are still one group");
    }

    /// The define path's two calls (interpreter round i1 wave 37, lane L5):
    /// nothing for an unconstrained `(name, loader)`, and no row grown for it;
    /// the pin a constraint already carries, and a violation that leaves it.
    #[test]
    fn the_define_path_sees_only_constrained_rows() {
        let mut c = LoaderConstraints::new();
        assert!(c.is_empty());
        assert_eq!(c.pinned_for("S", 1), None);
        assert!(c.pin_if_constrained("S", 1, 10).is_none());
        assert!(c.is_empty(), "an unconstrained load adds no row");
        assert!(c.impose("S", 1, 2).is_none());
        assert!(c.pin_if_constrained("S", 1, 10).is_none());
        assert_eq!(c.pinned_for("S", 2), Some(10));
        assert_eq!(c.pinned_for("S", 3), None);
        let v = c.pin_if_constrained("S", 2, 20).expect("violation");
        assert_eq!((v.existing, v.conflicting), (10, 20));
        assert_eq!(c.pinned_for("S", 1), Some(10), "the pin stays");
        assert!(c.pin_if_constrained("T", 2, 20).is_none());
        assert_eq!(c.tracked_pairs(), 2);
        c.set_label(2, LoaderLabel { name_and_id: "'b' @1".into(), parent: Some("'app'".into()) });
        assert_eq!(c.label(2).map(|l| l.name_and_id.as_str()), Some("'b' @1"));
        c.forget(&|l| l == 2, &|_| false);
        assert!(c.label(2).is_none(), "a dead loader's label goes with its rows");
        assert_eq!(c.tracked_pairs(), 1);
    }

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
