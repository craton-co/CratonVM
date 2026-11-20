//! Round 13 wave 4, lane mega3 (`r13w2-mega-dead-loader-selectors-patch`):
//! a dead loader's selectors are forgotten, their ids are never reissued,
//! and the ways keyed on them are withdrawn.
use super::*;
use crate::{ExecutableBuffer, JitCache};

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
    let cid = cratonvm_types::ClassId::new(0x1304);
    let class: Arc<str> = Arc::from("R13w4Mega3");
    let method: Arc<str> = Arc::from(name);
    let desc: Arc<str> = Arc::from("()V");
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    cache.get(&class, &method, &desc, cid).expect("published")
}

#[test]
fn a_dead_loaders_selectors_are_forgotten_and_never_reissued() {
    let table = MegaDispatchTable::new();
    let loader = mega_selector_loader_context;
    let s7 = table
        .selector_id_in_context("p/A", "f", "()V", 0, loader(7), 0)
        .expect("interned");
    let s7g = table
        .selector_id_in_context("p/A", "g", "()V", 0, loader(7), 0)
        .expect("interned");
    let s8 = table
        .selector_id_in_context("p/A", "f", "()V", 0, loader(8), 0)
        .expect("interned");
    // A caller-CLASS context 7 is not loader 7.
    let c7 = table
        .selector_id("p/A", "f", "()V", 0, 7, 0)
        .expect("interned");
    assert_eq!(table.forget_loader_selectors(&|l| l == 7), 2);
    assert_eq!(
        table.forget_loader_selectors(&|l| l == 7),
        0,
        "already forgotten"
    );
    // Survivors keep their ids.
    assert_eq!(
        table.selector_id_in_context("p/A", "f", "()V", 0, loader(8), 0),
        Some(s8)
    );
    assert_eq!(table.selector_id("p/A", "f", "()V", 0, 7, 0), Some(c7));
    // A re-intern of the forgotten key gets an id no selector ever had.
    let again = table
        .selector_id_in_context("p/A", "f", "()V", 0, loader(7), 0)
        .expect("interned");
    for old in [s7, s7g, s8, c7] {
        assert_ne!(again, old, "a forgotten id was reissued");
    }
    assert!(again > s7g && again > s8 && again > c7, "ids are monotonic");
}

#[test]
fn a_dead_loaders_ways_and_cells_are_withdrawn() {
    let cache = JitCache::new();
    let body = published(&cache, "fl-1");
    let entry = body.entry_ptr() as u64;
    let table = MegaDispatchTable::new();
    let loader = mega_selector_loader_context;
    let dead = table
        .selector_id_in_context("p/A", "f", "()V", 0, loader(7), 0)
        .expect("interned");
    let live = table
        .selector_id_in_context("p/A", "f", "()V", 0, loader(8), 0)
        .expect("interned");
    table.install_with_class_slot(5, dead, Some((2, 4)), entry, false, false);
    table.install_with_class_slot(6, live, Some((2, 4)), entry, false, false);
    assert!(table.lookup(5, dead).is_some() && table.lookup_class_cell(5, dead, 2).is_some());
    assert_eq!(table.forget_loader_selectors(&|l| l == 7), 1);
    assert!(
        table.lookup(5, dead).is_none(),
        "the dead selector's hashed way"
    );
    assert_eq!(
        table.lookup_class_cell(5, dead, 2),
        None,
        "the dead selector's cell"
    );
    assert!(
        table.lookup(6, live).is_some(),
        "another loader's way survives"
    );
    assert_eq!(table.lookup_class_cell(6, live, 2), Some((entry, false)));
    assert_eq!(table.class_cells_and_index_rows(), (1, 1));
    table.clear_entries();
    crate::defer_jit_owner(Some(body));
}

/// A directory replaced by a grown copy is buried, not kept, and freed
/// once its stamp is graced (`r12w4-mega3-shared-table-never-reclaims`
/// item 2); with the switch off every directory stays, as before. The
/// execution token keeps any other test's drain from gracing the stamp.
#[test]
fn a_replaced_directory_is_freed_once_graced() {
    let cache = JitCache::new();
    let body = published(&cache, "fl-dir");
    let entry = body.entry_ptr() as u64;
    let execution = crate::jit_execution_enter();
    for reclaim in [true, false] {
        let mut table = MegaDispatchTable::new();
        table.reclaim_old_dirs = reclaim;
        // The empty birth directory, then one covering class 3, then one
        // covering class 5000.
        table.install_with_class_slot(3, 1, Some((1, 1)), entry, false, false);
        table.install_with_class_slot(5000, 1, Some((1, 1)), entry, false, false);
        {
            let mut state = table.writer.lock();
            let kept = if reclaim { 1 } else { 3 };
            assert_eq!(state.dirs.len(), kept, "reclaim {reclaim}");
            assert_eq!(state.dir_graveyard.len(), 3 - kept, "buried, not freed");
            MegaDispatchTable::sweep_graveyard_locked(&mut state, |_| false);
            assert_eq!(state.dir_graveyard.len(), 3 - kept);
            MegaDispatchTable::sweep_graveyard_locked(&mut state, |_| true);
            assert!(state.dir_graveyard.is_empty());
            assert_eq!(state.dirs.len(), kept, "the current directory stays");
        }
        assert!(table.lookup_class_cell(3, 1, 1).is_some());
        assert!(table.lookup_class_cell(5000, 1, 1).is_some());
        table.clear_entries();
    }
    crate::jit_execution_leave(execution);
    crate::defer_jit_owner(Some(body));
}

/// M13-3: the inline-cache stack check reads the thread's floor word only
/// when the VM published its displacement, the leaf floor helper exists
/// (the check's exact side) and the thread mirror has a displacement;
/// anything less keeps the frame-slot form (`None`).
#[test]
fn the_floor_word_is_used_only_when_fully_wired() {
    let mut helpers = crate::JitRuntimeHelpers::default();
    assert_eq!(ic_stack_floor_word_offset(&helpers), None, "nothing wired");
    helpers.jit_stack_floor_offset_in_thread = 0x2A8;
    assert_eq!(
        ic_stack_floor_word_offset(&helpers),
        None,
        "no leaf floor helper"
    );
    helpers.native_stack_floor_fn = 0x1150;
    let expect = (crate::x64::jit_thread_tls_disp() != 0).then_some(0x2A8);
    assert_eq!(ic_stack_floor_word_offset(&helpers), expect);
    helpers.jit_stack_floor_offset_in_thread = 0;
    assert_eq!(
        ic_stack_floor_word_offset(&helpers),
        None,
        "switch off: 0 published"
    );
}
