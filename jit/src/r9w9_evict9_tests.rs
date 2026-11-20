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

fn entry_of(cache: &JitCache, class: &str, name: &str, id: u32) -> usize {
    cache
        .get(class, name, "()V", cratonvm_types::ClassId::new(id))
        .expect("published")
        .entry_ptr() as usize
}

/// A caller body that bakes `callee_entry` through a retire cell, as the
/// compile doors build it: the entry itself is a baked callee (so it is
/// rooted), and the artifact owns the cell.
fn caller_of(callee_entry: usize, class: &str) -> (CompiledMethod, Arc<JitCycleEdgeCell>) {
    let mut a = body(true);
    let cell = JitCycleEdgeCell::new_retire(callee_entry, class, "b", "()V", true);
    a._jit_cycle_cells.push(Arc::clone(&cell));
    a._direct_callee_entries = vec![callee_entry];
    (a, cell)
}

/// `for (i = 0; i < 10; i++) b();` -- the invoke at pc 8 is inside the
/// loop, the `return` at 17 is not.
///
/// ```text
///  0 iconst_0   1 istore_0   2 iload_0   3 bipush 10   5 if_icmpge +12 (17)
///  8 invokestatic #1   11 iinc 0,1   14 goto -12 (2)   17 return
/// ```
const LOOP_CALLER: [u8; 20] = [
    0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0c, 0xb8, 0x00, 0x01, 0x84, 0x00, 0x01, 0xa7,
    0xff, 0xf4, 0xb1, 0x00, 0x00,
];

#[test]
fn pc_in_loop_sees_the_backward_branch_range_only() {
    assert!(bytecode_pc_in_loop(&LOOP_CALLER, 18, 8));
    assert!(bytecode_pc_in_loop(&LOOP_CALLER, 18, 2), "the header");
    assert!(bytecode_pc_in_loop(&LOOP_CALLER, 18, 14), "the back edge");
    assert!(!bytecode_pc_in_loop(&LOOP_CALLER, 18, 0));
    assert!(!bytecode_pc_in_loop(&LOOP_CALLER, 18, 17));
    // invokestatic #1; return -- no loop at all.
    assert!(!bytecode_pc_in_loop(&[0xb8, 0x00, 0x01, 0xb1], 4, 0));
}



/// A retire cell is never a key's waiting cell: publishing a body of the
/// method its names give does not fill an unfilled one.
#[test]
fn a_retire_cell_is_not_filled_by_key() {
    let cache = JitCache::new();
    let (mut a, cell) = caller_of(0x7100_0000_0000, "r9w9c/B");
    // Nothing to root: the cell stays at `0` (the site dispatches).
    a._direct_callee_entries.clear();
    put(&cache, "r9w9c/A", "a", 94, a);
    assert_eq!(cell.current_entry(), 0);
    put(&cache, "r9w9c/B", "b", 95, body(true));
    assert_eq!(cell.current_entry(), 0, "a retire cell waits for no key");
    cache.clear_all();
}

