use super::*;

#[test]
fn t1x_an_unload_forgets_the_osr_memo_rows_of_its_classes() {
    let bridge = JitBridgeState::default();
    if let Ok(mut refused) = bridge.osr_optimizing_refused.lock() {
        refused.insert((7, 1, 10), 0);
        refused.insert((7, 1, usize::MAX), 0);
        refused.insert((8, 1, 10), 0);
    }
    assert_eq!(bridge.forget_osr_optimizing_of_unloaded(&|id| id == 7), 0);
    let left: Vec<u32> = bridge
        .osr_optimizing_refused
        .lock()
        .map(|r| r.keys().map(|k| k.0).collect())
        .unwrap_or_default();
    assert_eq!(left, vec![8], "only the live class keeps its rows");
}

/// The unload path calls it, after the class-keyed gate memo.
#[test]
fn t1x_the_unload_path_forgets_the_osr_memo() {
    let gc: String = include_str!("../../../memory/gc.rs")
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let gate = gc.find(".jit_gate_pass.write().retain(");
    let memo = gc.find(".forget_osr_optimizing_of_unloaded(&|id|unloaded_ids.contains(&id));");
    assert!(gate.is_some() && gate < memo, "gate {gate:?} memo {memo:?}");
}
