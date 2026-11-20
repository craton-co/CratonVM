use super::*;
use cratonvm_types::ClassId;

const A: ClassId = ClassId::new(0x7A36_C101);
const A2: ClassId = ClassId::new(0x7A36_C102);
const B: ClassId = ClassId::new(0x7A36_C103);
const C: ClassId = ClassId::new(0x7A36_C104);
const NONE: ClassId = ClassId::new(0);

/// Two loaders' `w36c/A`, an identified and an id-less `w36c/B` and
/// `w36c/C`.
const ROWS: [(ClassId, &str); 6] = [
    (A, "w36c/A"),
    (A2, "w36c/A"),
    (B, "w36c/B"),
    (NONE, "w36c/B"),
    (C, "w36c/C"),
    (NONE, "w36c/C"),
];

/// The empty set, an absent class, one class, and a mixed set with an
/// id-less member.
const SETS: [&[(ClassId, &str)]; 4] = [
    &[],
    &[(ClassId::new(0x7A36_C1F8), "w36c/Absent")],
    &[(A, "w36c/A")],
    &[(A, "w36c/A"), (B, "w36c/B"), (NONE, "w36c/C")],
];

/// `owns` is `tiered::MethodKey::belongs_to` asked of every member, for
/// every subset of the rows as members and every row as the key.
#[test]
fn owns_is_belongs_to_asked_of_every_member() {
    for mask in 0u32..(1 << ROWS.len()) {
        let members: Vec<(ClassId, &str)> = ROWS
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, row)| *row)
            .collect();
        let set = UnloadedClassSet::new(members.iter().copied());
        assert_eq!(set.is_empty(), members.is_empty());
        for &(id, name) in &ROWS {
            let key = crate::tiered::MethodKey::with_class_id(id, name, "m", "()V");
            let expected = members
                .iter()
                .any(|&(mid, mname)| key.belongs_to(mid, mname));
            assert_eq!(
                set.owns(id, name),
                expected,
                "key {id:?} {name} in {members:?}"
            );
            assert_eq!(
                set.contains_nonzero_id(id),
                id.as_u32() != 0 && members.iter().any(|&(mid, _)| mid == id)
            );
        }
    }
}

#[test]
fn forget_for_classes_matches_a_loop_of_forget_for_class() {
    let fill = || {
        let vm = JitVerdictRegistry::default();
        for &(id, name) in &ROWS {
            vm.mark_bail_listed(id, name, "m", "()V");
        }
        vm
    };
    let state = |vm: &JitVerdictRegistry| -> Vec<bool> {
        ROWS.iter()
            .map(|&(id, name)| vm.is_bail_listed(id, name, "m", "()V"))
            .collect()
    };
    for set in SETS {
        let looped = fill();
        let looped_count: usize = set
            .iter()
            .map(|&(id, name)| looped.forget_for_class(id, name))
            .sum();
        let batched = fill();
        let batched_count =
            batched.forget_for_classes(&UnloadedClassSet::new(set.iter().copied()));
        assert_eq!(state(&looped), state(&batched), "set {set:?}");
        assert_eq!(looped_count, batched_count, "set {set:?}");
        assert_eq!(looped.bail_list_size(), batched.bail_list_size());
    }
    let batched = fill();
    assert_eq!(
        batched.forget_for_classes(&UnloadedClassSet::new(SETS[3].iter().copied())),
        5
    );
    assert_eq!(state(&batched), [false, true, false, false, false, false]);
}

#[test]
fn forget_class_ids_matches_a_loop_of_forget_class_id() {
    let fill = || {
        let mut cycles = CycleSet::default();
        for &(id, name) in &ROWS {
            cycles.insert(JitCompileMethodKey::of_class(id, name, "m", "()V"));
        }
        cycles
    };
    let members = |cycles: &CycleSet| {
        let mut rows: Vec<(u32, String)> = cycles
            .by_names
            .values()
            .flatten()
            .map(|key| (key.class_id.as_u32(), key.class_name.clone()))
            .collect();
        rows.sort();
        rows
    };
    for set in SETS {
        let mut looped = fill();
        let looped_count: usize = set.iter().map(|&(id, _)| looped.forget_class_id(id)).sum();
        let mut batched = fill();
        let batched_count =
            batched.forget_class_ids(&UnloadedClassSet::new(set.iter().copied()));
        assert_eq!(members(&looped), members(&batched), "set {set:?}");
        assert_eq!(looped_count, batched_count, "set {set:?}");
    }
    // Exact non-zero identity only: the mixed set drops `A` and `B`, and
    // keeps the other loader's `A` and every id-less member.
    let mut batched = fill();
    assert_eq!(
        batched.forget_class_ids(&UnloadedClassSet::new(SETS[3].iter().copied())),
        2
    );
    assert_eq!(batched.len(), 4);
    let registry = JitVerdictRegistry::default();
    assert_eq!(
        registry.forget_cycle_members_of_unloaded_classes(&UnloadedClassSet::new(
            SETS[3].iter().copied()
        )),
        0
    );
}

fn body(inlined: &[(&str, &str, &str)]) -> CompiledMethod {
    let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    cm.inlined_methods = inlined
        .iter()
        .map(|(c, m, d)| (c.to_string(), m.to_string(), d.to_string()))
        .collect();
    cm
}

/// `(class, method, declaring id)` of every body `fill_cache` publishes.
const BODIES: [(&str, &str, ClassId); 6] = [
    ("w36c/A", "m", A),
    ("w36c/A", "m", A2),
    ("w36c/Caller", "inl", ClassId::new(0x7A36_C105)),
    ("w36c/Direct", "call", ClassId::new(0x7A36_C106)),
    ("w36c/Other", "o", ClassId::new(0x7A36_C107)),
    ("w36c/Z", "z", NONE),
];

/// Body 2 inlined from `w36c/B`; body 3 bakes a direct call into body 0,
/// so it goes with body 0 through the reverse closure.
fn fill_cache() -> JitCache {
    let cache = JitCache::new();
    let mut direct_callee = 0usize;
    for (i, &(class, method, id)) in BODIES.iter().enumerate() {
        let mut cm = match i {
            2 => body(&[("w36c/B", "x", "()V")]),
            _ => body(&[]),
        };
        if i == 0 {
            direct_callee = cm.entry_ptr() as usize;
        }
        if i == 3 {
            cm._direct_callee_entries.push(direct_callee);
        }
        cache.put(class.into(), method.into(), "()V".into(), id, cm);
    }
    cache
}

fn live_bodies(cache: &JitCache) -> Vec<bool> {
    BODIES
        .iter()
        .map(|&(class, method, id)| cache.get(class, method, "()V", id).is_some())
        .collect()
}

#[test]
fn invalidate_unloaded_classes_matches_a_loop_of_invalidate_unloaded_class() {
    for set in SETS {
        let looped = fill_cache();
        assert_eq!(live_bodies(&looped), [true; 6], "every body published");
        let looped_count: usize = set
            .iter()
            .map(|&(id, name)| looped.invalidate_unloaded_class(id, name))
            .sum();
        let batched = fill_cache();
        let batched_count =
            batched.invalidate_unloaded_classes(&UnloadedClassSet::new(set.iter().copied()));
        assert_eq!(live_bodies(&looped), live_bodies(&batched), "set {set:?}");
        assert_eq!(looped_count, batched_count, "set {set:?}");
    }
    // The mixed set: `A`'s body and its direct caller, the body that
    // inlined from `B`, and (by plain equality with the id-less member,
    // as the per-class form does) the id-less body.
    let batched = fill_cache();
    assert_eq!(
        batched.invalidate_unloaded_classes(&UnloadedClassSet::new(SETS[3].iter().copied())),
        4
    );
    assert_eq!(
        live_bodies(&batched),
        [false, true, false, false, true, false]
    );
}

