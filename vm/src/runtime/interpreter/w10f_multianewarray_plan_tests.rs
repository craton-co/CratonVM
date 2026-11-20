use super::*;

fn plan(components: &[u32]) -> Arc<MultiANewArrayPlan> {
    Arc::new(MultiANewArrayPlan {
        leaf_et: ArrayElementType::Reference,
        total_array_depth: components.len() + 1,
        component_ids: components.iter().map(|&id| ClassId::new(id)).collect(),
    })
}

/// gc-common w10-f: one VM's unload drops only its own plans that name an
/// unloaded class, on either side, and leaves every other VM's plans --
/// including one on the same ids -- alone.
#[test]
fn an_unload_drops_only_its_vms_plans_that_name_an_unloaded_class() {
    const VM: usize = 0x10F_1A00;
    const OTHER: usize = 0x10F_1B00;
    const GONE: u32 = 70;
    // addr-keyed: test-only VM identifiers, not object addresses.
    let mut plans: rustc_hash::FxHashMap<(usize, u32, u16, u8), Arc<MultiANewArrayPlan>> =
        Default::default();
    plans.insert((VM, GONE, 1, 2), plan(&[10, 11])); // key names it
    plans.insert((VM, 5, 2, 2), plan(&[GONE, 11])); // a component names it
    plans.insert((VM, 5, 3, 2), plan(&[10, 11])); // names only live ids
    plans.insert((OTHER, GONE, 1, 2), plan(&[GONE, 11])); // another VM's ids
    plans.insert((VM, 6, 4, 1), plan(&[])); // primitive leaf, no components

    retain_multianewarray_plans(&mut plans, VM, &|id| id.as_u32() == GONE);
    let mut left: Vec<(usize, u32, u16, u8)> = plans.keys().copied().collect();
    left.sort_unstable();
    assert_eq!(
        left,
        vec![(VM, 5, 3, 2), (VM, 6, 4, 1), (OTHER, GONE, 1, 2)],
        "only VM's plans naming the unloaded id go"
    );

    // The teardown form drops the rest of VM's plans and nothing else.
    retain_multianewarray_plans(&mut plans, VM, &|_| true);
    let left: Vec<(usize, u32, u16, u8)> = plans.keys().copied().collect();
    assert_eq!(left, vec![(OTHER, GONE, 1, 2)]);
}
