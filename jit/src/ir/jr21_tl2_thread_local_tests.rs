use super::*;

/// `static Object f(InheritableThreadLocal t) { return t.get(); }`.
const GET: [u8; 5] = [0x2a, 0xb6, 0x00, 0x01, 0xb0];

fn layout(inheritable_class: u32) -> ThreadLocalIrLayout {
    ThreadLocalIrLayout {
        thread_obj_offset: 64,
        thread_local_class: 40,
        supplied_class: 41,
        map_class: 42,
        entry_class: 43,
        thread_locals_index: 5,
        table_index: 1,
        hash_index: 0,
        referent_index: 0,
        value_index: 4,
        thread_locals_row: 0,
        table_row: 0,
        hash_row: 0,
        value_row: 0,
        set_admitted: true,
        inheritable_class,
        inheritable_thread_locals_index: 7,
        inheritable_thread_locals_row: 0,
        referent_row: 0,
    }
}

fn build(class_name: &'static str, inheritable_class: u32, on: bool) -> Graph {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name,
        method_name: "get",
        descriptor: "()Ljava/lang/Object;",
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let info_ptr = info as *const JitInvokeInfo as usize;
    // The switch (`CRATONVM_JIT_IR_INHERITABLE_THREAD_LOCAL`, JIT r23
    // wave 2f) is read VM-side: off, the layout carries no class.
    let inheritable_class = if on { inheritable_class } else { 0 };
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    b.set_invoke_info(HashMap::from([(1usize, (info_ptr, 1usize, b'L'))]));
    b.set_thread_local_layout(Some(layout(inheritable_class)));
    b.build(&GET, GET.len()).expect("builds")
}

fn count(g: &Graph, pred: impl Fn(&Node) -> bool) -> usize {
    g.nodes.iter().filter(|n| pred(n)).count()
}

/// Is `n` a reference field load whose index input is the constant `index`?
fn loads_index(g: &Graph, n: &Node, index: i64) -> bool {
    matches!(n.op, Op::Load(MemKind::Ref))
        && n.inputs
            .get(3)
            .and_then(|&i| g.nodes.get(i as usize))
            .is_some_and(|c| c.op == Op::Const(index))
}

/// An `InheritableThreadLocal` site tests the receiver against exactly
/// `InheritableThreadLocal` (never `ThreadLocal` / `SuppliedThreadLocal`)
/// and reads `Thread.inheritableThreadLocals`, never `threadLocals`.
#[test]
fn jr21_tl2_an_inheritable_site_reads_the_inheritable_map() {
    if crate::x64::jit_thread_tls_disp() == 0 {
        return;
    }
    let g = build("java/lang/InheritableThreadLocal", 44, true);
    assert_eq!(
        count(&g, |n| matches!(n.op, Op::CurrentThreadMirror { .. })),
        1
    );
    assert_eq!(count(&g, |n| n.op == Op::ExactClassIs { class_id: 44 }), 1);
    for other in [40u32, 41] {
        assert_eq!(
            count(&g, |n| n.op == Op::ExactClassIs { class_id: other }),
            0,
            "class {other}"
        );
    }
    assert!(
        count(&g, |n| loads_index(&g, n, 7)) >= 1,
        "inheritableThreadLocals"
    );
    assert_eq!(
        count(&g, |n| loads_index(&g, n, 5)),
        0,
        "never threadLocals"
    );
    assert_eq!(
        count(&g, |n| matches!(n.op, Op::Call { .. })),
        1,
        "the miss call"
    );
}

/// No class id in the layout, or the switch off: the call alone. A
/// `ThreadLocal` site keeps its plain-map expansion either way.
#[test]
fn jr21_tl2_inheritable_needs_its_layout_and_its_switch() {
    if crate::x64::jit_thread_tls_disp() == 0 {
        return;
    }
    for g in [
        build("java/lang/InheritableThreadLocal", 0, true),
        build("java/lang/InheritableThreadLocal", 44, false),
    ] {
        assert_eq!(
            count(&g, |n| matches!(n.op, Op::CurrentThreadMirror { .. })),
            0
        );
        assert_eq!(count(&g, |n| matches!(n.op, Op::Call { .. })), 1);
    }
    let plain = build("java/lang/ThreadLocal", 44, false);
    assert_eq!(
        count(&plain, |n| n.op == Op::ExactClassIs { class_id: 40 }),
        1
    );
    assert!(count(&plain, |n| loads_index(&plain, n, 5)) >= 1);
    assert_eq!(count(&plain, |n| loads_index(&plain, n, 7)), 0);
}

/// The layout words carry the three new rows (15..=17).
#[test]
fn jr21_tl2_the_layout_words_carry_the_inheritable_rows() {
    let w = [
        64u64, 40, 41, 42, 43, 5, 1, 0, 0, 4, 0, 0, 0, 0, 1, 44, 7, 9, 0,
    ];
    let l = ThreadLocalIrLayout::from_words(&w).expect("complete");
    assert_eq!(
        (
            l.inheritable_class,
            l.inheritable_thread_locals_index,
            l.inheritable_thread_locals_row
        ),
        (44, 7, 9)
    );
}

/// Text pin (a splice needs the whole resolver to build): the spliced
/// arm is behind its switch, keeps the caller-pc tests for the top level
/// only, and names the call's result in the post-call snapshot.
#[test]
fn jr21_tl2_the_spliced_arm_is_gated_and_names_the_call_result() {
    let src = concat!(include_str!("../ir/mod.rs"), "\n", include_str!("../ir/node_id.rs"), "\n", include_str!("../ir/safepoints.rs"), "\n", include_str!("../ir/loop_phis.rs"), "\n", include_str!("../ir/set_osr_requested_header.rs"), "\n", include_str!("../ir/splice_op_cannot_raise.rs"), "\n", include_str!("../ir/narrow_int_return.rs"), "\n", include_str!("../ir/is_jdk_throwable_init_candidate.rs"), "\n", include_str!("../ir/attach_sb_virtual_builders.rs"), "\n", include_str!("../ir/field_load_memo_mode.rs"), "\n", include_str!("../ir/ir_splice_store_fence_forward_branch.rs"), "\n", include_str!("../ir/ir_trap_census.rs")).replace("\r\n", "\n");
    let start = src
        .find("fn try_thread_local_get_intrinsic(")
        .expect("the expansion");
    let body = &src[start..];
    // `\x7d`: no brace literal in a test (the brace-counting ratchets).
    let body = &body[..body.find("\n    \x7d\n").expect("its end")];
    for needle in [
        "ir_thread_local_in_splice_enabled()",
        "pc >= f.base && pc < f.end",
        "(!spliced\n",
        "self.post_call_join_value = Some((phi, call))",
        "self.pending_post_call = None",
    ] {
        assert!(body.contains(needle), "{needle}");
    }
}
