use super::*;
use std::sync::Arc;

fn body(with_context: bool) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    if with_context {
        CompiledMethod::new_with_context(buf)
    } else {
        CompiledMethod::new(buf)
    }
}

fn put(cache: &JitCache, class: &str, name: &str, id: u32, cm: CompiledMethod) {
    cache.put(
        Arc::from(class),
        Arc::from(name),
        Arc::from("()V"),
        cratonvm_types::ClassId::new(id),
        cm,
    );
}

fn entry_of(cache: &JitCache, class: &str, name: &str, id: u32) -> u64 {
    cache
        .get(class, name, "()V", cratonvm_types::ClassId::new(id))
        .expect("published")
        .entry_ptr() as u64
}

fn cell_for(class: &str, id: u32) -> Arc<JitCycleEdgeCell> {
    JitCycleEdgeCell::new(class, "a", "()V", cratonvm_types::ClassId::new(id), true)
}

/// Generated code loads `[cell + JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET]`.
#[test]
fn the_entry_word_sits_at_the_offset_the_codegen_loads() {
    let cell = cell_for("t/A", 1);
    let base = Arc::as_ptr(&cell) as usize;
    let word = &cell.entry_word as *const std::sync::atomic::AtomicU64 as usize;
    assert_eq!(word - base, JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET as usize);
    assert_eq!(cell.current_entry(), 0, "a fresh cell dispatches");
}

/// The closing caller publishes first (it was compiled inside its
/// ancestor's compile); the ancestor's publication fills the cell, a
/// tier-up re-fills it with the newer body, and an invalidation of the
/// target sends the site back to dispatch.
#[test]
fn put_fills_a_waiting_cell_and_an_invalidation_clears_it() {
    let cache = JitCache::new();
    let mut b = body(true);
    let cell = cell_for("r9w7/A", 41);
    b._jit_cycle_cells.push(Arc::clone(&cell));
    put(&cache, "r9w7/B", "b", 42, b);
    assert_eq!(
        cell.current_entry(),
        0,
        "the ancestor has not published yet"
    );

    put(&cache, "r9w7/A", "a", 41, body(true));
    let a1 = entry_of(&cache, "r9w7/A", "a", 41);
    assert_eq!(
        cell.current_entry(),
        a1,
        "the ancestor's publication fills the cell"
    );

    put(&cache, "r9w7/A", "a", 41, body(true));
    let a2 = entry_of(&cache, "r9w7/A", "a", 41);
    assert_eq!(cell.current_entry(), a2, "the newest body wins");

    cache.remove("r9w7/A", "a", "()V", cratonvm_types::ClassId::new(41));
    assert_eq!(
        cell.current_entry(),
        0,
        "a withdrawn target must not stay callable"
    );
    assert!(
        cache
            .get("r9w7/B", "b", "()V", cratonvm_types::ClassId::new(42))
            .is_some(),
        "the closing caller itself stays published: its site dispatches again"
    );

    put(&cache, "r9w7/A", "a", 41, body(true));
    assert_eq!(cell.current_entry(), entry_of(&cache, "r9w7/A", "a", 41));
    cache.clear_all();
    assert_eq!(cell.current_entry(), 0, "a flush clears every cell");
}

/// A target that is already published when the closing caller publishes
/// fills the cell at registration.
#[test]
fn a_cell_whose_target_is_already_published_is_filled_at_registration() {
    let cache = JitCache::new();
    put(&cache, "r9w7r/A", "a", 51, body(true));
    let a = entry_of(&cache, "r9w7r/A", "a", 51);
    let mut b = body(true);
    let cell = cell_for("r9w7r/A", 51);
    b._jit_cycle_cells.push(Arc::clone(&cell));
    put(&cache, "r9w7r/B", "b", 52, b);
    assert_eq!(cell.current_entry(), a);
    cache.clear_all();
}

/// Only a body the site can call raw is ever installed: another loader's
/// same-named class, a body with the other entry ABI, and a body that
/// needs the synchronized wrapper all leave the cell alone.
#[test]
fn a_cell_admits_only_its_own_class_and_entry_abi() {
    let cache = JitCache::new();
    let mut b = body(true);
    let cell = cell_for("r9w7x/A", 61);
    b._jit_cycle_cells.push(Arc::clone(&cell));
    put(&cache, "r9w7x/B", "b", 62, b);

    put(&cache, "r9w7x/A", "a", 63, body(true));
    assert_eq!(cell.current_entry(), 0, "same name, another class id");

    put(&cache, "r9w7x/A", "a", 61, body(false));
    assert_eq!(
        cell.current_entry(),
        0,
        "a context-free body has the other ABI"
    );

    let mut wrapped = body(true);
    wrapped.requires_wrapped_entry = true;
    put(&cache, "r9w7x/A", "a", 61, wrapped);
    assert_eq!(
        cell.current_entry(),
        0,
        "a synchronized body needs its wrapper"
    );

    put(&cache, "r9w7x/A", "a", 61, body(true));
    assert_eq!(cell.current_entry(), entry_of(&cache, "r9w7x/A", "a", 61));
    cache.clear_all();
}

#[test]
fn any_abi_cell_accepts_both_entry_abis() {
    let cache = JitCache::new();
    let mut b = body(true);
    let cell = JitCycleEdgeCell::new_any_abi(
        "r9w7x/A",
        "a",
        "()V",
        cratonvm_types::ClassId::new(71),
    );
    b._jit_cycle_cells.push(Arc::clone(&cell));
    put(&cache, "r9w7x/B", "b", 72, b);
    assert_eq!(cell.current_entry(), 0);

    put(&cache, "r9w7x/A", "a", 71, body(false));
    assert_eq!(cell.current_entry(), entry_of(&cache, "r9w7x/A", "a", 71));

    put(&cache, "r9w7x/A", "a", 71, body(true));
    assert_eq!(
        cell.current_entry(),
        entry_of(&cache, "r9w7x/A", "a", 71) | crate::JIT_IC_NEEDS_CONTEXT_TAG
    );
    cache.clear_all();
}

