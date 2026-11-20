use super::{
    ir_osr_guard_exit_entries_enabled, lower_inner_with_mode_exit_polls, IrPollModeExits,
    PollExitBytecode,
};
use crate::deopt::{DeoptReason, DeoptimizationPoint};
use crate::ir::{IrBuilder, IrInlineFrameSites};
use crate::{CompiledMethod, JitInvokeInfo, JitRuntimeHelpers};
use std::collections::{HashMap, HashSet};

/// `static void f() { for (int i = 0; i < 100; i++) g(); }` with
/// `static void g()`: the header at 2, the call at 8, its successor (the
/// `iinc`) at 11.
const LOOP_CALL: [u8; 20] = [
    0x03, // 0:  iconst_0
    0x3b, // 1:  istore_0
    0x1a, // 2:  iload_0             <- loop header
    0x10, 0x64, // 3:  bipush 100
    0xa2, 0x00, 0x0c, // 5:  if_icmpge +12 -> 17
    0xb8, 0x00, 0x01, // 8:  invokestatic #1 g()V
    0x84, 0x00, 0x01, // 11: iinc 0, 1
    0xa7, 0xff, 0xf4, // 14: goto -12 -> 2
    0xb1, // 17: return
    0, 0,
];

/// The slow path the lowered body would call; never run here.
extern "C" fn never_called(_flag_addr: i64, _body_id: i64) -> i64 {
    0
}

fn lower(mode: IrPollModeExits) -> CompiledMethod {
    // Leaked: `Op::Call` bakes the address and the lowerer reads it.
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/Loop",
        method_name: "g",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let info_ptr = info as *const JitInvokeInfo as usize;
    let mut builder = IrBuilder::new(0, 1);
    let mut invoke_info = HashMap::new();
    invoke_info.insert(8usize, (info_ptr, 0usize, b'V'));
    builder.set_invoke_info(invoke_info);
    let graph = builder
        .build(&LOOP_CALL, LOOP_CALL.len() - 2)
        .expect("IR build of a loop with a call");
    let schedule = crate::ir_schedule::schedule(&graph);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
    // `usize`, so all-zero is a valid value; the test wires the poll and
    // the dispatch helper, and runs nothing.
    let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    // A leaked flag byte (no `static`: `process_global_statics_ratchet`).
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    helpers.safepoint_flag_addr = flag as *const u8 as usize; // Cast: baked flag address
    helpers.safepoint_slow_path = never_called as *const () as usize; // Cast: helper address
    helpers.invoke_dispatch = 0x1111_2222_3333_4440;
    lower_inner_with_mode_exit_polls(
        &crate::DirectHelperTable::EMPTY,
        &graph,
        &schedule,
        0,
        1,
        &helpers,
        &HashMap::new(),
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &IrInlineFrameSites::default(),
        &HashMap::new(),
        &HashSet::new(),
        None,
        &[],
        Some(&[]),
        mode,
        Some(PollExitBytecode {
            code: &LOOP_CALL,
            code_len: LOOP_CALL.len() - 2,
            num_params: 0,
            num_locals: 1,
            handlers: &[],
        }),
        None,
    )
    .expect("the loop with a call lowers")
}

/// The `OsrExit` points at `bci` of `cm`.
fn exits_at(cm: &CompiledMethod, bci: u32) -> usize {
    cm._deopt_point_boxes
        .iter()
        .filter(|p| p.reason == DeoptReason::OsrExit && p.bci == bci)
        .count()
}


/// Interpreter round i1 wave 38, lane L3 (`i24-L6-...`, "the optimizing
/// tier's method-entry bodies"): a METHOD-ENTRY compile of the same loop
/// gets the same site -- its point at the call's successor, a frame the
/// stash sinks' predicate resumes -- and the force pass accepts it. Before,
/// only an OSR-door compile got one. When the header's own method-entry
/// exit is refused on this host, the call's may be too.
#[test]
fn a_method_entry_compile_puts_a_forceable_exit_after_a_call() {
    if !crate::deopt_real_enabled() {
        return;
    }
    let entry = lower(IrPollModeExits::MethodEntry);
    if entry.compile_id == 0 {
        assert!(entry.post_call_exit_sites.is_empty());
        return;
    }
    if exits_at(&entry, 2) == 0 && entry.post_call_exit_sites.is_empty() {
        return;
    }
    assert_eq!(
        entry.post_call_exit_sites.len(),
        1,
        "one real call, one site"
    );
    assert_eq!(
        exits_at(&entry, 11),
        1,
        "its point resumes at the successor"
    );
    let point: &DeoptimizationPoint = entry
        ._deopt_point_boxes
        .iter()
        .find(|p| p.reason == DeoptReason::OsrExit && p.bci == 11)
        .expect("the site's point");
    assert!(
        point.frame_state.stack.is_empty(),
        "a void call pushes nothing"
    );
    assert_eq!(point.frame_state.locals.len(), 1, "`i`");
    assert!(crate::osr_exit::guard_exit_point_resumable(point));
    let (site, stub) = entry.post_call_exit_sites[0];
    // Cast: buffer offsets, widened.
    let (site, stub) = (site as usize, stub as usize);
    assert_eq!(
        &entry.code_bytes()[site..site + 5],
        &crate::ENTRY_PATCH_PAD[..],
        "an unwritten NOP"
    );
    assert_eq!(
        entry.code_bytes()[stub],
        0xE8,
        "the stub opens with the CALL"
    );
    let forced = entry
        .force_exit_polls()
        .expect("the force pass accepts the site");
    assert!(forced >= 1);
    assert_eq!(entry.code_bytes()[site], 0xE9, "JMP rel32 to its stub");
}

/// `static void h(Object o) { synchronized (o) { for (int i = 0; i < 100;
/// i++) g(); } }` as javac emits it: the header at 6 and the call at 12
/// inside the block, the call's successor (the `iinc`) at 15, the
/// block's catch-all handler at 26. `max_locals` 4.
const CALL_IN_LOCK: [u8; 34] = [
    0x2a, // 0:  aload_0
    0x59, // 1:  dup
    0x4c, // 2:  astore_1
    0xc2, // 3:  monitorenter
    0x03, // 4:  iconst_0
    0x3d, // 5:  istore_2
    0x1c, // 6:  iload_2             <- loop header
    0x10, 0x64, // 7:  bipush 100
    0xa2, 0x00, 0x0c, // 9:  if_icmpge +12 -> 21
    0xb8, 0x00, 0x01, // 12: invokestatic #1 g()V
    0x84, 0x02, 0x01, // 15: iinc 2, 1
    0xa7, 0xff, 0xf4, // 18: goto -12 -> 6
    0x2b, // 21: aload_1
    0xc3, // 22: monitorexit
    0xa7, 0x00, 0x08, // 23: goto +8 -> 31
    0x4e, // 26: astore_3            <- handler, any
    0x2b, // 27: aload_1
    0xc3, // 28: monitorexit
    0x2d, // 29: aload_3
    0xbf, // 30: athrow
    0xb1, // 31: return
    0, 0,
];
/// `[4, 23) -> 26` and `[26, 29) -> 26`, both catch-all.
const CALL_IN_LOCK_HANDLERS: [(usize, usize, usize); 2] = [(4, 23, 26), (26, 29, 26)];

extern "C" fn fake_monitor(_vm: i64, obj: i64) -> i64 {
    obj
}

extern "C" fn fake_monitor_exit(_vm: i64, _obj: i64) -> i64 {
    1
}



/// `static void f() { for (int i = 0; i < 100; i++) a(); }` with `a()`
/// spliced at the call (bci 8) from `static void a() { g(); }` (combined
/// pc 18: `invokestatic g()V; return`), `g` a real call. The caller's
/// header at 2, `a`'s call at its own bci 0, its successor (the `return`)
/// at `a`'s bci 3 = combined pc 21.
const SPLICED_CALL: [u8; 24] = [
    0x03, // 0:  iconst_0
    0x3b, // 1:  istore_0
    0x1a, // 2:  iload_0             <- loop header
    0x10, 0x64, // 3:  bipush 100
    0xa2, 0x00, 0x0c, // 5:  if_icmpge +12 -> 17
    0xb8, 0x00, 0x01, // 8:  invokestatic #1 a()V   (spliced)
    0x84, 0x00, 0x01, // 11: iinc 0, 1
    0xa7, 0xff, 0xf4, // 14: goto -12 -> 2
    0xb1, // 17: return
    0xb8, 0x00, 0x02, // 18: a: invokestatic #2 g()V
    0xb1, // 21: a: return
    0, 0,
];
const SPLICED_OUTER: &str = "pkg/Loop.f:()V";
const SPLICED_A: &str = "pkg/Loop.a:()V";




/// The scope test of [`chain_scopes_have_kept_sources`]: every spliced
/// scope's key must be kept; the outermost (the compiling method) need
/// not be; a flat frame is no chain.
#[test]
fn a_chain_is_covered_only_when_every_spliced_scope_is_kept() {
    use super::chain_scopes_have_kept_sources;
    use crate::deopt::FrameState;
    let frame = |key: &str, caller: Option<FrameState>| FrameState {
        class_id: 0,
        method_key: key.to_string(),
        bci: 0,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: caller.map(Box::new),
    };
    let outer = frame("O.m:()V", None);
    let mid = frame("A.a:()V", Some(outer.clone()));
    let inner = frame("B.b:()V", Some(mid));
    let keys = |ks: &[&str]| ks.iter().map(|k| (*k).to_string()).collect::<Vec<_>>();
    assert!(chain_scopes_have_kept_sources(
        &inner,
        &keys(&["A.a:()V", "B.b:()V"])
    ));
    assert!(!chain_scopes_have_kept_sources(&inner, &keys(&["B.b:()V"])));
    assert!(!chain_scopes_have_kept_sources(&inner, &keys(&["A.a:()V"])));
    assert!(!chain_scopes_have_kept_sources(&outer, &keys(&["O.m:()V"])));
}


/// `static void f() { for (int i = 0; i < 100; i++) a(); }` with `a()`
/// spliced at the call (bci 8) from
/// `static void a() { for (int j = 0; j < 10; j++) { } }` (combined pc
/// 18): `a`'s own loop header at its bci 2 (combined pc 20), its `goto`
/// back edge at its bci 11 (combined 29). Interpreter round i2 wave 57,
/// lane L2.
const SPLICED_LOOP: [u8; 36] = [
    0x03, // 0:  iconst_0
    0x3b, // 1:  istore_0
    0x1a, // 2:  iload_0             <- f's loop header
    0x10, 0x64, // 3:  bipush 100
    0xa2, 0x00, 0x0c, // 5:  if_icmpge +12 -> 17
    0xb8, 0x00, 0x01, // 8:  invokestatic #1 a()V   (spliced)
    0x84, 0x00, 0x01, // 11: iinc 0, 1
    0xa7, 0xff, 0xf4, // 14: goto -12 -> 2
    0xb1, // 17: return
    0x03, // 18: a 0:  iconst_0
    0x3b, // 19: a 1:  istore_0
    0x1a, // 20: a 2:  iload_0       <- a's loop header
    0x10, 0x0a, // 21: a 3:  bipush 10
    0xa2, 0x00, 0x09, // 23: a 5:  if_icmpge +9 -> a 14
    0x84, 0x00, 0x01, // 26: a 8:  iinc 0, 1
    0xa7, 0xff, 0xf7, // 29: a 11: goto -9 -> a 2
    0xb1, // 32: a 14: return
    0, 0, 0,
];


/// The chain poll exit of `SPLICED_LOOP`'s inner loop: `a` at its own
/// header (bci 2) over `f` parked in its `invoke` (bci 8), or `None`.
fn spliced_header_exit(cm: &CompiledMethod) -> Option<&DeoptimizationPoint> {
    cm._deopt_point_boxes
        .iter()
        .map(|p| &**p)
        .find(|p| p.reason == DeoptReason::OsrExit && p.frame_state.caller.is_some())
}



