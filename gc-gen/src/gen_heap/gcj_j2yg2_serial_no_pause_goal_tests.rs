use super::*;

const MIB: usize = 1024 * 1024;

fn goal(edits: &[(&str, Option<&str>)]) -> u64 {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        GenerationalHeap::with_capacity(64 * MIB).young_pause_goal_ms
    })
}

