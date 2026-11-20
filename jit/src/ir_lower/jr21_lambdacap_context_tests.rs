use super::*;

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
    // `usize`, so all-zero is a valid value; nothing here calls a helper.
    unsafe { std::mem::zeroed() }
}

/// `int f(int a) { return a + 1; }`, optionally marked as a graph whose
/// capturing proxy escape analysis has already removed (only the class
/// set is left: no `Op::New`, no store, no call).
fn pure_body(proxy_built: bool) -> Graph {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
    let sum = graph.add(Op::Add, IrType::Int, vec![a, one], None);
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
    if proxy_built {
        graph.lambda_proxy_alloc_classes.insert(0x8000_0021);
    }
    graph
}


