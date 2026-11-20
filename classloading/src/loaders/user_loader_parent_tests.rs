use super::*;

// One table per test: the registry is per VM since wave 103.
#[test]
fn ancestors_walk_nearest_first_and_stop_at_the_builtin_chain() {
    let parents = UserLoaderParents::new();
    parents.register(9001, 9002);
    parents.register(9002, 9003);
    parents.register(9003, 0);
    let mut out = [0u32; MAX_USER_LOADER_DEPTH];
    let n = parents.ancestors(9001, &mut out);
    assert_eq!(&out[..n], &[9002, 9003]);
    assert!(parents.parent_known(9003));
    assert!(!parents.parent_known(9004));
    assert_eq!(parents.builtin_parent(9001), Some(0));
}

#[test]
fn a_cycle_terminates() {
    let parents = UserLoaderParents::new();
    parents.register(9101, 9102);
    parents.register(9102, 9101);
    let mut out = [0u32; MAX_USER_LOADER_DEPTH];
    let n = parents.ancestors(9101, &mut out);
    assert_eq!(&out[..n], &[9102]);
    assert_eq!(parents.builtin_parent(9101), None);
}

#[test]
fn builtin_and_self_parents_are_not_walked() {
    let parents = UserLoaderParents::new();
    parents.register(9201, 9201);
    let mut out = [0u32; MAX_USER_LOADER_DEPTH];
    assert_eq!(parents.ancestors(9201, &mut out), 0);
    assert_eq!(parents.ancestors(2, &mut out), 0);
    assert!(!parents.has_any());
}

/// Wave 103: two VMs' tables are separate (the process global before
/// answered every VM with any VM's links).
#[test]
fn i103_two_tables_are_separate() {
    let a = UserLoaderParents::new();
    let b = UserLoaderParents::new();
    a.register(7, 1);
    assert!(a.parent_known(7));
    assert!(!b.parent_known(7));
    assert!(!b.has_any());
    assert_eq!(a.builtin_parent(7), Some(1));
    assert_eq!(b.builtin_parent(7), None);
}
