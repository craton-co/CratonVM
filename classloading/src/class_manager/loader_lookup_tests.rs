use super::*;

/// The O(1) `name_definitions` index must answer exactly what the linear
/// `unique_loaded_class` scan it replaced answers -- including after
/// redefinition (same key, new id) and multi-loader aliasing (two keys,
/// same id, which still reads as unique).
#[test]
fn unique_definition_index_matches_linear_scan() {
    // Mirrors ClassManager::{loaded_classes_insert, loaded_classes_remove}
    // against a bare map; `rebuilt` is the from-scratch index the unload
    // path's per-alias release must also agree with (gc-common w18-d).
    #[derive(Default)]
    struct Harness {
        map: LoadedClassesMap,
        index: FxHashMap<Arc<str>, FxHashMap<ClassId, u32>>,
    }
    impl Harness {
        fn insert(&mut self, loader: ClassLoaderId, name: &str, id: u32) {
            let name: Arc<str> = Arc::from(name);
            let id = ClassId::new(id);
            if let Some(old) = self.map.insert((loader, Arc::clone(&name)), id) {
                release_name_definition(&mut self.index, &name, old);
            }
            *self.index.entry(name).or_default().entry(id).or_insert(0) += 1;
        }
        fn remove(&mut self, loader: ClassLoaderId, name: &str) {
            let name: Arc<str> = Arc::from(name);
            if let Some(old) = self.map.remove(&(loader, Arc::clone(&name))) {
                release_name_definition(&mut self.index, &name, old);
            }
        }
        fn unique_via_index(&self, name: &str) -> Option<ClassId> {
            let ids = self.index.get(name)?;
            let mut distinct = ids.keys();
            let id = *distinct.next()?;
            distinct.next().is_none().then_some(id)
        }
        fn rebuilt(&self) -> FxHashMap<Arc<str>, FxHashMap<ClassId, u32>> {
            let mut index: FxHashMap<Arc<str>, FxHashMap<ClassId, u32>> = FxHashMap::default();
            for ((_, name), &id) in self.map.iter() {
                *index
                    .entry(Arc::clone(name))
                    .or_default()
                    .entry(id)
                    .or_insert(0) += 1;
            }
            index
        }
        /// The condition `loaded_classes_insert` computes to raise
        /// `ANY_DUPLICATE_CLASS_NAME`: "some name resolves to more than
        /// one distinct `ClassId`".
        fn duplicate_via_index(&self) -> bool {
            self.index.values().any(|ids| ids.len() > 1)
        }
        /// The property that condition is supposed to mean, computed the
        /// slow way straight off the map: two live entries sharing a name
        /// but not an id. This is exactly the situation
        /// `retarget_instance_field_to_receiver` exists to handle, so if
        /// these two ever disagree the field gate is unsound.
        fn duplicate_linear(&self) -> bool {
            for ((_, a_name), &a_id) in self.map.iter() {
                for ((_, b_name), &b_id) in self.map.iter() {
                    if a_name == b_name && a_id != b_id {
                        return true;
                    }
                }
            }
            false
        }
        fn check(&self, names: &[&str]) {
            for name in names {
                assert_eq!(
                    self.unique_via_index(name),
                    unique_loaded_class(&self.map, name),
                    "index disagrees with the linear scan for {name}"
                );
            }
            assert_eq!(self.index, self.rebuilt(), "incremental index drifted");
            assert_eq!(
                self.duplicate_via_index(),
                self.duplicate_linear(),
                "the duplicate-name gate condition disagrees with the \
                 property it stands for; `any_duplicate_class_name` would \
                 let `retarget_instance_field_to_receiver` skip a receiver \
                 it must retarget"
            );
        }
    }

    let names = ["p/X", "p/Y", "p/Z", "p/Absent"];
    let mut h = Harness::default();
    h.check(&names);

    // One definition -> unique.
    h.insert(ClassLoaderId::Bootstrap, "p/X", 1);
    h.check(&names);

    // Two loaders, SAME id (delegation alias) -> still unique.
    h.insert(ClassLoaderId::Application, "p/X", 1);
    h.check(&names);

    // Two loaders, DIFFERENT ids -> ambiguous, must answer None.
    h.insert(ClassLoaderId::UserDefined(7), "p/X", 2);
    assert_eq!(h.unique_via_index("p/X"), None);
    h.check(&names);

    // Dropping the odd one out restores uniqueness.
    h.remove(ClassLoaderId::UserDefined(7), "p/X");
    assert_eq!(h.unique_via_index("p/X"), Some(ClassId::new(1)));
    h.check(&names);

    // Redefinition: same key, new id. The displaced id must not linger.
    h.insert(ClassLoaderId::Bootstrap, "p/X", 3);
    h.insert(ClassLoaderId::Application, "p/X", 3);
    assert_eq!(h.unique_via_index("p/X"), Some(ClassId::new(3)));
    h.check(&names);

    // Independent name, then full removal -> name retired from the index.
    h.insert(ClassLoaderId::Bootstrap, "p/Y", 4);
    h.check(&names);
    h.remove(ClassLoaderId::Bootstrap, "p/Y");
    assert!(!h.index.contains_key("p/Y"));
    h.check(&names);

    // Removing a key that was never inserted is a no-op.
    h.remove(ClassLoaderId::Extension, "p/Z");
    h.check(&names);
}

fn definitions(entries: &[(ClassLoaderId, &str, u32)]) -> LoadedClassesMap {
    entries
        .iter()
        .map(|(loader, name, id)| ((*loader, Arc::<str>::from(*name)), ClassId::new(*id)))
        .collect()
}

#[test]
fn built_in_lookup_is_parent_first_and_never_delegates_down() {
    let all = definitions(&[
        (ClassLoaderId::Bootstrap, "p/X", 1),
        (ClassLoaderId::Extension, "p/X", 2),
        (ClassLoaderId::Application, "p/X", 3),
    ]);
    assert_eq!(
        loaded_class_for_requesting_loader(&all, &crate::loaders::UserLoaderParents::new(), ClassLoaderId::Application, "p/X", true),
        Some(ClassId::new(1))
    );
    assert_eq!(
        loaded_class_for_requesting_loader(&all, &crate::loaders::UserLoaderParents::new(), ClassLoaderId::Extension, "p/X", true),
        Some(ClassId::new(1))
    );

    let app_only = definitions(&[(ClassLoaderId::Application, "p/Y", 4)]);
    assert_eq!(
        loaded_class_for_requesting_loader(&app_only, &crate::loaders::UserLoaderParents::new(), ClassLoaderId::Bootstrap, "p/Y", true),
        None
    );
    assert_eq!(
        loaded_class_for_requesting_loader(&app_only, &crate::loaders::UserLoaderParents::new(), ClassLoaderId::Extension, "p/Y", true),
        None
    );
}

#[test]
fn user_lookup_never_scans_an_unrelated_user_namespace() {
    let first = ClassLoaderId::UserDefined(11);
    let second = ClassLoaderId::UserDefined(12);
    let map = definitions(&[
        (ClassLoaderId::Application, "p/X", 1),
        (first, "p/X", 2),
        (second, "p/Y", 3),
    ]);
    assert_eq!(
        loaded_class_for_requesting_loader(&map, &crate::loaders::UserLoaderParents::new(), first, "p/X", true),
        Some(ClassId::new(2))
    );
    assert_eq!(
        loaded_class_for_requesting_loader(&map, &crate::loaders::UserLoaderParents::new(), first, "p/X", false),
        Some(ClassId::new(1))
    );
    assert_eq!(
        loaded_class_for_requesting_loader(&map, &crate::loaders::UserLoaderParents::new(), first, "p/Y", true),
        None
    );
}

#[test]
fn context_free_lookup_fails_closed_on_loader_ambiguity() {
    let map = definitions(&[
        (ClassLoaderId::Application, "p/X", 1),
        (ClassLoaderId::UserDefined(11), "p/X", 2),
        (ClassLoaderId::UserDefined(12), "p/Y", 3),
    ]);
    assert_eq!(unique_loaded_class(&map, "p/X"), None);
    assert_eq!(unique_loaded_class(&map, "p/Y"), Some(ClassId::new(3)));
    assert_eq!(unique_loaded_class(&map, "p/Z"), None);
}
