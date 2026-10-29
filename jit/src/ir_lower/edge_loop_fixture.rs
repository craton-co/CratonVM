//! JIT rx27 (lane regalloc): an executed pressure loop for the regalloc
//! switches' tests (`operand_in_place`, `publish_reload_elide`).
//! It came from the caller-saved EBB tests (wave 5), which were removed with
//! `CRATONVM_JIT_IR_LS_CALLER_SAVED_EBB`.

use super::*;
use crate::ir::IrBuilder;

/// ```java
/// static int f(int n) {
///     int k = n * 3, m = n + 7;
///     int a = 0, b = 1, c = 2;
///     for (int i = 0; i < n; i++) {
///         int v = i * 5 + k;              // compared in its block, read in the next
///         if (v > m) a += v ^ m;
///         b += a + i;
///         c ^= b + k;
///     }
///     return a + b + c;
/// }
/// ```
///
/// Three carried accumulators, the counter and two invariants hold the
/// callee-saved file; `v` is compared in one block and read in the next.
const EDGE_LOOP: [u8; 81] = [
    0x1a, 0x06, 0x68, 0x3c, // 0: k = n * 3
    0x1a, 0x10, 0x07, 0x60, 0x3d, // 4: m = n + 7
    0x03, 0x3e, // 9: a = 0
    0x04, 0x36, 0x04, // 11: b = 1
    0x05, 0x36, 0x05, // 14: c = 2
    0x03, 0x36, 0x06, // 17: i = 0
    0x15, 0x06, 0x1a, 0xa2, 0x00, 0x30, // 20: if (i >= n) goto 71
    0x15, 0x06, 0x08, 0x68, 0x1b, 0x60, 0x36, 0x07, // 26: v = i * 5 + k
    0x15, 0x07, 0x1c, 0xa4, 0x00, 0x0a, // 34: if (v <= m) goto 47
    0x1d, 0x15, 0x07, 0x1c, 0x82, 0x60, 0x3e, // 40: a += v ^ m
    0x15, 0x04, 0x1d, 0x15, 0x06, 0x60, 0x60, 0x36, 0x04, // 47: b += a + i
    0x15, 0x05, 0x15, 0x04, 0x1b, 0x60, 0x82, 0x36, 0x05, // 56: c ^= b + k
    0x84, 0x06, 0x01, // 65: i++
    0xa7, 0xff, 0xd0, // 68: goto 20
    0x1d, 0x15, 0x04, 0x60, 0x15, 0x05, 0x60, 0xac, // 71: return a + b + c
    0, 0,
];
const EDGE_LOOP_LEN: usize = 79;

pub(super) fn edge_ref(n: i64) -> i32 {
    // Casts: Java `int` arithmetic, wrapping.
    let n = n as i32;
    let (k, m) = (n.wrapping_mul(3), n.wrapping_add(7));
    let (mut a, mut b, mut c) = (0i32, 1i32, 2i32);
    let mut i = 0i32;
    while i < n {
        let v = i.wrapping_mul(5).wrapping_add(k);
        if v > m {
            a = a.wrapping_add(v ^ m);
        }
        b = b.wrapping_add(a.wrapping_add(i));
        c ^= b.wrapping_add(k);
        i += 1;
    }
    a.wrapping_add(b).wrapping_add(c)
}

extern "C" fn never_poll(_flag_addr: i64, _body_id: i64) -> i64 {
    0
}

fn graph_and_schedule() -> (Graph, Schedule) {
    let mut builder = IrBuilder::new(1, 8);
    builder.set_param_types(&[IrType::Int]);
    let mut graph = builder.build(&EDGE_LOOP, EDGE_LOOP_LEN).expect("IR build");
    crate::ir_optimize::optimize(&mut graph);
    let schedule = crate::ir_schedule::schedule(&graph);
    (graph, schedule)
}

pub(super) fn lower_edge_loop(edits: &[(&str, Option<&str>)]) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let (graph, schedule) = graph_and_schedule();
        // A leaked poll word (no `static`): never set.
        let word: &'static mut [u64; 2] = Box::leak(Box::new([0, 0]));
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
        // `usize`, so all-zero is a valid value; only the poll is wired.
        let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers.safepoint_flag_addr = word.as_ptr() as usize; // Cast: baked flag address
        helpers.safepoint_slow_path = never_poll as *const () as usize; // Cast: helper address
        lower_inner(
            &graph,
            &schedule,
            1,
            8,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
        )
        .expect("the loop lowers")
    })
}

pub(super) fn run(cm: &CompiledMethod, n: i64) -> i32 {
    // SAFETY: an `int f(int)` loop body that calls nothing (the poll word is
    // never set).
    let got = unsafe { cm.try_call_with_context(0, &[n]) }.expect("call");
    // Cast: this tier's `int` result is the low 32 bits of RAX.
    got as i32
}
