use super::*;
use crate::ir::{Graph, IrType, Op};
use crate::ir_schedule;
use std::cell::Cell;
use std::sync::atomic::Ordering;

/// The resolved target, two proxies the fake helper answers `true` for
/// and one it answers `false` for.
const TARGET: u32 = 0x0004_7A01;
const P1: u32 = 0x8000_7A01;
const P2: u32 = 0x8000_7A02;
const P3: u32 = 0x8000_7A03;

thread_local! {
    static CALLS: Cell<usize> = Cell::new(0);
}

fn calls() -> usize {
    CALLS.with(Cell::get)
}

fn passes(obj: i64) -> bool {
    if obj == 0 {
        return false;
    }
    // SAFETY: every non-null receiver here is a live fake object of this
    // module whose first word holds its class id.
    let id = unsafe { std::ptr::read_volatile(obj as *const u32) }; // Cast: fake reference
    id == TARGET || id == P1 || id == P2
}

extern "C" fn fake_instanceof(_vm: i64, obj: i64, _name: i64, _len: i64) -> i64 {
    CALLS.with(|c| c.set(c.get() + 1));
    i64::from(passes(obj))
}

/// Returns the reference, as `jit_checkcast` does on a pass; the tests
/// cast only receivers that pass.
extern "C" fn fake_checkcast(_vm: i64, obj: i64, _name: i64, _len: i64) -> i64 {
    CALLS.with(|c| c.set(c.get() + 1));
    obj
}

/// A plain fake object: the class id is the `u32` at offset 0, and the
/// rest of the header word (the mark word, whose kind byte is
/// `KIND_TAGS_BYTE_OFFSET`, 6) stays 0.
fn object(class_id: u32) -> Vec<u64> {
    vec![u64::from(class_id), 0, 0, 0]
}

fn addr(o: &[u64]) -> i64 {
    o.as_ptr() as i64 // Cast: object address as JIT argument
}

fn check_graph(op: Op, ty: IrType) -> Graph {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let o = graph.add(Op::Param(0), IrType::Ref, vec![start], Some(0));
    let check = graph.add(op, ty, vec![ctrl, mem, o], Some(1));
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, check], Some(4));
    graph
}

fn helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
    // `usize`, so all-zero is a valid value; only the two type-check
    // helpers are reached.
    let mut h: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.instanceof_check = fake_instanceof as *const () as usize;
    h.checkcast = fake_checkcast as *const () as usize;
    h
}

fn lower_with(graph: &Graph, overrides: &[(&str, Option<&str>)]) -> CompiledMethod {
    let schedule = ir_schedule::schedule(graph);
    cratonvm_types::flags::with_thread_overrides(overrides, || {
        lower(graph, &schedule, 1, 1, &helpers())
    })
    .expect("the type check lowers")
}

fn run(cm: &CompiledMethod, obj: i64) -> i64 {
    // SAFETY: a body whose only reachable helpers are the two fakes above,
    // over live fake objects it only reads.
    unsafe { cm.try_call_with_context(0, &[obj]) }.expect("call")
}

fn latch_raised() -> bool {
    cratonvm_types::duplicate_class_name_seen()
}



/// The fill bytes, run: one way overwrites word 0; two ways move word 0
/// into word 2 first. Both bases.
#[test]
fn the_hit_way_fill_moves_the_previous_proxy_to_the_second_way() {
    for base_rcx in [false, true] {
        for two in [false, true] {
            let mut buf = ExecutableBuffer::new(64).expect("buffer");
            // The arguments (cell, id) into the base register and R11.
            let windows = cfg!(target_os = "windows");
            let id_mov: &[u8] = if windows {
                &[0x49, 0x89, 0xD3] // MOV R11, RDX
            } else {
                &[0x49, 0x89, 0xF3] // MOV R11, RSI
            };
            let base_mov: &[u8] = match (base_rcx, windows) {
                (false, true) => &[0x48, 0x89, 0xC8],  // MOV RAX, RCX
                (true, true) => &[],                   // RCX already
                (false, false) => &[0x48, 0x89, 0xF8], // MOV RAX, RDI
                (true, false) => &[0x48, 0x89, 0xF9],  // MOV RCX, RDI
            };
            buf.emit(id_mov);
            buf.emit(base_mov);
            buf.emit(&proxy_hit_way_fill_bytes(base_rcx, two));
            buf.emit(&[0xC3]); // RET
            buf.finalize();
            // SAFETY: the function just emitted: it writes words 0 and 2
            // of the cell it is given, touches only volatile registers and
            // returns.
            let f: extern "C" fn(*const crate::TypecheckSeenCell, u64) =
                unsafe { std::mem::transmute(buf.as_ptr()) };
            let cell: Box<crate::TypecheckSeenCell> = Box::default();
            f(&*cell, u64::from(P1));
            f(&*cell, u64::from(P2));
            let word = |i: usize| cell[i].load(Ordering::Relaxed);
            let label = format!("base_rcx={base_rcx} two={two}");
            assert_eq!(word(0), P2, "{label}: the last proxy in way 0");
            assert_eq!(word(1), 0, "{label}: the miss way is untouched");
            assert_eq!(word(2), if two { P1 } else { 0 }, "{label}: the second hit way");
            assert_eq!(word(3), 0, "{label}: padding");
        }
    }
}


// The lowered `checkcast` arm: off, every proxy cast calls; on, a passed
// proxy is passed inline afterwards (both hit ways with the second
// switch), the reference comes back unchanged, and a cast writes no miss.

