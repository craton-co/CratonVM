use super::*;

fn member() -> cratonvm_native_builtins::lang_invoke::MhDirectMember {
    cratonvm_native_builtins::lang_invoke::MhDirectMember {
        ref_kind: 6,
        class_id: 9,
        class: "p/Impl".to_string(),
        name: "twice".to_string(),
        desc: "(I)I".to_string(),
        declared: "(I)I".to_string(),
    }
}

#[test]
fn the_rows_of_unloaded_holders_go_and_the_others_stay() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    {
        let mut rows = shared.jit.bridge.mh_condy_members.lock().unwrap();
        for key in [(1u32, 4u16), (2, 4), (2, 7), (3, 4)] {
            rows.insert(key, member());
        }
    }
    retire_mh_condy_members_of_unloaded(&shared, &rustc_hash::FxHashSet::default());
    assert_eq!(shared.jit.bridge.mh_condy_members.lock().unwrap().len(), 4, "empty batch");
    let dead: rustc_hash::FxHashSet<u32> = [2u32, 5].into_iter().collect();
    retire_mh_condy_members_of_unloaded(&shared, &dead);
    let rows = shared.jit.bridge.mh_condy_members.lock().unwrap();
    let mut left: Vec<(u32, u16)> = rows.keys().copied().collect();
    left.sort_unstable();
    assert_eq!(left, vec![(1, 4), (3, 4)], "holder 2's two rows are dropped");
}

#[test]
fn the_pure_rule_counts_what_it_removed() {
    let mut rows: std::collections::HashMap<(u32, u16), ()> =
        [((1, 1), ()), ((1, 2), ()), ((4, 1), ())].into_iter().collect();
    let dead: rustc_hash::FxHashSet<u32> = [1u32].into_iter().collect();
    assert_eq!(retain_condy_rows_of_live_holders(&mut rows, &dead), 2);
    assert_eq!(rows.len(), 1);
    assert!(rows.contains_key(&(4, 1)));
}

#[test]
fn the_class_unload_path_calls_it_with_the_batch() {
    let gc: String = include_str!("../../../memory/gc.rs")
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    assert!(
        gc.contains("jit_bridge::retire_mh_condy_members_of_unloaded(shared,&class_ids"),
        "unload_dead_class_metadata retires the unloaded holders' rows"
    );
}
