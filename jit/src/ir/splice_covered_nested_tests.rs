// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 7 (lane T1L1, CF23-1): a nested body kept spliced inside a
//! spliced callee's covered `try` (`splice_covered_nested`).

use super::splice_covered_nested::{covered_nested_chain, proven_exit_nodes_from};
use super::*;
use std::collections::HashMap;

const GUARDED: &str = concat!("CRATON", "VM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED");
const PURE: &str = concat!("CRATON", "VM_JIT_IR_SPLICE_COVERED_NESTED_PURE");
const PROVEN: &str = concat!("CRATON", "VM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN");
const NEW_NON_ERROR: &str = concat!("CRATON", "VM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH");

// ── The catch-type whitelist ─────────────────────────────────────────

#[test]
fn only_listed_jdk_exceptions_cannot_catch_an_error() {
    assert!(catch_type_cannot_catch_error(
        "java/lang/IllegalArgumentException"
    ));
    assert!(catch_type_cannot_catch_error(
        "java.lang.IllegalArgumentException"
    ));
    assert!(catch_type_cannot_catch_error("java/lang/RuntimeException"));
    for name in [
        "java/lang/Throwable",
        "java/lang/Error",
        "java/lang/OutOfMemoryError",
        "java/lang/LinkageError",
        "",
        "p/MyException",
        "java/lang/IllegalArgumentExceptio",
    ] {
        assert!(!catch_type_cannot_catch_error(name), "{name:?}");
    }
}

// ── The chain predicate and the proven-node list ─────────────────────

#[test]
fn a_chain_is_covered_when_a_parent_covers_its_childs_invoke() {
    // Outermost first: (base, invoke pc). Frame 1 was opened at 7 inside
    // frame 0's body (base 5); frame 2 at 23 inside frame 1's (base 20).
    let frames = [(5usize, 1usize), (20, 7), (40, 23)];
    let covers = |ranges: &'static [(usize, usize, usize)]| {
        move |base: usize, pc: usize| {
            ranges
                .iter()
                .any(|&(b, s, e)| b == base && s <= pc && pc < e)
        }
    };
    assert!(
        covered_nested_chain(&frames, covers(&[(5, 5, 12)])),
        "frame 0 covers 7"
    );
    assert!(
        covered_nested_chain(&frames, covers(&[(20, 22, 24)])),
        "frame 1 covers 23"
    );
    assert!(
        !covered_nested_chain(&frames, covers(&[(5, 8, 12)])),
        "7 is outside [8, 12)"
    );
    assert!(
        !covered_nested_chain(&frames, covers(&[(40, 0, 99)])),
        "the innermost body's own rows cover none of its parents' invokes"
    );
    assert!(
        !covered_nested_chain(&frames[..1], covers(&[(5, 0, 99)])),
        "one frame"
    );
}

#[test]
fn the_proven_list_is_the_field_accesses_and_allocations_after_the_mark() {
    let mut g = Graph::empty(8);
    let before = g.add(Op::Load(MemKind::Int), IrType::Int, vec![], Some(1));
    let mark = g.nodes.len();
    let load = g.add(Op::Load(MemKind::Ref), IrType::Ref, vec![], Some(2));
    let _add = g.add(Op::Add, IrType::Int, vec![], Some(2));
    let store = g.add(Op::Store(MemKind::Int), IrType::Void, vec![], Some(2));
    let new = g.add(
        Op::New {
            class_id: 9,
            num_fields: 1,
        },
        IrType::Ref,
        vec![],
        Some(2),
    );
    let arr = g.add(Op::ArrayLoad(MemKind::Int), IrType::Int, vec![], Some(2));
    let len = g.add(Op::ArrayLength, IrType::Int, vec![], Some(2));
    let synthetic_arr = g.add(Op::ArrayLoad(MemKind::Int), IrType::Int, vec![], None);
    let ids = proven_exit_nodes_from(&g.nodes, mark);
    assert_eq!(ids, vec![load, store, new, arr, len]);
    assert!(!ids.contains(&synthetic_arr), "a pc-less access has no deopt resume site");
    assert!(!ids.contains(&before));
}

#[test]
fn the_lowering_admits_a_recorded_node_only_under_its_switch() {
    let mut g = Graph::empty(2);
    let load = g.add(Op::Load(MemKind::Int), IrType::Int, vec![], Some(2));
    let other = g.add(Op::Load(MemKind::Int), IrType::Int, vec![], Some(3));
    let array_load = g.add(Op::ArrayLoad(MemKind::Int), IrType::Int, vec![], Some(4));
    let array_length = g.add(Op::ArrayLength, IrType::Int, vec![], Some(5));
    g.splice_handler_proven_exits.push(load);
    g.splice_handler_proven_exits.push(array_load);
    g.splice_handler_proven_exits.push(array_length);
    let ask = |v: Option<&str>, id: NodeId| {
        cratonvm_types::flags::with_thread_overrides(&[(PROVEN, v)], || {
            splice_handler_exit_proven(&g, id)
        })
    };
    assert!(ask(Some("1"), load));
    assert!(ask(Some("1"), array_load));
    assert!(ask(Some("1"), array_length));
    assert!(!ask(Some("1"), other), "not proven by the builder");
    assert!(ask(None, load), "default on");
    assert!(!ask(Some("0"), load));
}

#[test]
fn the_lowering_proves_only_direct_initialized_static_reads() {
    let mut g = Graph::empty(2);
    let direct = g.add(
        Op::LoadStatic {
            class_id: 9,
            field_index: 2,
            type_tag: b'I',
            is_volatile: false,
        },
        IrType::Int,
        vec![],
        Some(2),
    );
    let helper = g.add(
        Op::LoadStatic {
            class_id: 9,
            field_index: 3,
            type_tag: b'I',
            is_volatile: false,
        },
        IrType::Int,
        vec![],
        Some(3),
    );
    let volatile = g.add(
        Op::LoadStatic {
            class_id: 9,
            field_index: 2,
            type_tag: b'I',
            is_volatile: true,
        },
        IrType::Int,
        vec![],
        Some(4),
    );
    g.plain_static_reads.insert((9, 2));
    let ask = |v: Option<&str>, id: NodeId| {
        cratonvm_types::flags::with_thread_overrides(&[(PROVEN, v)], || {
            splice_handler_exit_proven(&g, id)
        })
    };
    assert!(ask(Some("1"), direct));
    assert!(!ask(Some("1"), helper), "helper static access may run clinit");
    assert!(!ask(Some("1"), volatile), "volatile access stays on the helper");
    assert!(ask(None, direct), "default on");
    assert!(!ask(Some("0"), direct));
}

// ── The planner admission ────────────────────────────────────────────

/// `this.f = x; return;` -- `aload_0; iload_1; putfield #5; return`.
fn setter() -> crate::InlineSite {
    crate::InlineSite {
        callee_code: vec![0x2a, 0x1b, 0xb5, 0x00, 0x05, 0xb1, 0, 0],
        callee_code_len: 6,
        class_id: 7,
        ..Default::default()
    }
}

fn row(site: crate::InlineSite) -> crate::NestedInlineSite {
    crate::NestedInlineSite {
        callee_pc: 4,
        site,
        ..Default::default()
    }
}

fn admitted(n: &crate::NestedInlineSite) -> bool {
    cratonvm_types::flags::with_thread_overrides(&[(GUARDED, Some("1"))], || {
        covered_nested_row_admitted(n)
    })
}

#[test]
fn a_setter_body_is_admitted_only_under_the_switch() {
    let n = row(setter());
    assert!(admitted(&n));
    assert!(cratonvm_types::flags::with_thread_overrides(
        &[(GUARDED, None)],
        || { covered_nested_row_admitted(&n) }
    ));
    for v in [Some("0")] {
        assert!(
            !cratonvm_types::flags::with_thread_overrides(&[(GUARDED, v)], || {
                covered_nested_row_admitted(&n)
            }),
            "{v:?}: opt-in"
        );
    }
}

#[test]
fn a_body_with_an_exit_of_its_own_is_refused() {
    let edit = |at: usize, op: u8| {
        let mut s = setter();
        s.callee_code[at] = op;
        row(s)
    };
    assert!(!admitted(&edit(1, 0xbb)), "new");

    assert!(!admitted(&edit(1, 0xb2)), "getstatic");

    // A back edge: `goto -2` at 2.
    let mut s = setter();
    s.callee_code = vec![0x2a, 0x00, 0xa7, 0xff, 0xfe, 0xb1, 0, 0];
    assert!(!admitted(&row(s)), "a backward branch");
    let mut s = setter();
    s.callee_is_synchronized = true;
    assert!(!admitted(&row(s)), "synchronized");
    let mut s = setter();
    s.ir_handler_rows = vec![(0, 2, 5, "java/lang/Throwable".to_string(), 7)];
    assert!(!admitted(&row(s)), "a table of its own");
    let mut s = setter();
    s.class_id = 0;
    assert!(!admitted(&row(s)), "no identity");
    let mut n = row(setter());
    n.guard_class_id = 11;
    assert!(admitted(&n), "a guarded row now arms its miss call");
}

/// `Sub.<init>(x)`: `aload_0; invokespecial Base.<init>; aload_0; iload_1;
/// putfield #5; return`, the super call at pc 1.
fn ctor(nested: Vec<crate::NestedInlineSite>, elided: Vec<usize>) -> crate::NestedInlineSite {
    row(crate::InlineSite {
        callee_code: vec![
            0x2a, 0xb7, 0x00, 0x09, 0x2a, 0x1b, 0xb5, 0x00, 0x05, 0xb1, 0, 0,
        ],
        callee_code_len: 10,
        class_id: 7,
        nested_sites: nested.into(),
        elided_invoke_pcs: elided,
        ..Default::default()
    })
}

#[test]
fn covered_nested_plans_stop_at_the_depth_limit() {
    let n = row(setter());
    cratonvm_types::flags::with_thread_overrides(&[(GUARDED, Some("1"))], || {
        assert!(splice_covered_nested::covered_nested_row_admitted_at_depth(
            &n, 0
        ));
        assert!(splice_covered_nested::covered_nested_row_admitted_at_depth(
            &n, 7
        ));
        assert!(!splice_covered_nested::covered_nested_row_admitted_at_depth(&n, 8));
    });
}

#[test]
fn an_invoke_is_admitted_only_into_an_admitted_body_or_an_elided_one() {
    assert!(
        !admitted(&ctor(vec![], vec![])),
        "a call would be an unarmed exit"
    );
    assert!(admitted(&ctor(vec![], vec![1])), "Object.<init> elided");
    let mut sup = row(setter());
    sup.callee_pc = 1;
    assert!(
        admitted(&ctor(vec![sup.clone()], vec![])),
        "the super body spliced too"
    );
    sup.site.callee_code[1] = 0xbb;
    assert!(!admitted(&ctor(vec![sup], vec![])), "transitively");
}

// ── The builder ──────────────────────────────────────────────────────

/// `static int g(int x) { int r; try { r = h(k(x)); } catch
/// (RuntimeException e) { throw e; } return r; }`: `[0, 7)` covers both
/// calls; `k` (at 1) is the nested body, `h` (at 4) a call with its edge.
const G: [u8; 16] = [
    0x1a, // 0: iload_0
    0xb8, 0x00, 0x02, // 1: invokestatic k(I)I
    0xb8, 0x00, 0x03, // 4: invokestatic h(I)I
    0x3c, // 7: istore_1
    0xa7, 0x00, 0x06, // 8: goto 14
    0x4d, // 11: astore_2 (handler)
    0x2c, // 12: aload_2
    0xbf, // 13: athrow
    0x1b, // 14: iload_1
    0xac, // 15: ireturn
];
/// `static int k(int x) { return x + 1; }`.
const K: [u8; 4] = [0x1a, 0x04, 0x60, 0xac];

fn call_row(method: &'static str) -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "p/Host",
        method_name: method,
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

fn inline_site(base: usize, len: usize, key: &str) -> IrInlineSite {
    IrInlineSite {
        base,
        code_len: len,
        num_args: 1,
        max_locals: 3,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: key.to_string(),
        class_id: 7,
    }
}

/// `static int f(int x) { return g(x); }` with `g` spliced at 1 and `k`
/// spliced inside `g`'s covered range.
fn build(edits: &[(&str, Option<&str>)]) -> Option<Graph> {
    build_with(&K, edits)
}

/// [`build`] with `k`'s body `k_code`.
fn build_with(k_code: &[u8], edits: &[(&str, Option<&str>)]) -> Option<Graph> {
    build_options(k_code, edits, false)
}

fn build_options(k_code: &[u8], edits: &[(&str, Option<&str>)], guarded: bool) -> Option<Graph> {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let caller: [u8; 5] = [0x1a, 0xb8, 0x00, 0x01, 0xac];
        let code_len = caller.len();
        let reference = guarded || k_code.contains(&0xbe);
        let mut g_code = G;
        if reference {
            g_code[0] = 0x2a;
        }
        if guarded {
            g_code[0] = 0x2a;
            g_code[1] = 0xb6;
        }
        let plan = splice_callee_handler_plan(&g_code, g_code.len(), &[(0, 7, 11)])?;
        let mut combined = caller.to_vec();
        if reference {
            combined[0] = 0x2a;
        }
        let base = combined.len();
        combined.extend_from_slice(&plan.code);
        let k_base = combined.len();
        combined.extend_from_slice(k_code);
        combined.extend_from_slice(&[0, 0]);
        let mut b = IrBuilder::new(1, 1);
        b.set_param_types(&[if reference { IrType::Ref } else { IrType::Int }]);
        b.graph.scope_method_key = "p/Host.f:(I)I".to_string();
        b.set_invoke_info(HashMap::from([(1usize, (call_row("g"), 1usize, b'I'))]));
        b.set_splice_handler_edges(true);
        let mut tables = IrInlineTables {
            sites: HashMap::from([
                (1usize, inline_site(base, G.len(), "p/Host.g:(I)I")),
                (base + 1, inline_site(k_base, k_code.len(), "p/Host.k:(I)I")),
            ]),
            invoke_info: HashMap::from([
                (base + 1, (call_row("k"), 1usize, b'I')),
                (base + 4, (call_row("h"), 1usize, b'I')),
            ]),
            splice_handler_rows: HashMap::from([(
                base,
                vec![IrSpliceHandlerRow {
                    start_pc: base,
                    end_pc: base + 7,
                    handler_pc: base + 11,
                    catch_name: "java/lang/RuntimeException",
                    declaring_class_id: 7,
                }],
            )]),
            ..IrInlineTables::default()
        };
        if guarded {
            let k = tables.sites.get_mut(&(base + 1))?;
            k.receiver_is_arg0 = true;
            k.class_id = 8;
            let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
                class_name: "p/Worker",
                method_name: "k",
                descriptor: "()I",
                num_jit_args: 1,
                return_type: b'I',
                invoke_kind: 0,
                declaring_class_id: 8,
                owner_class_id: 0,
            }));
            tables
                .invoke_info
                .insert(base + 1, (info as *const _ as usize, 1, b'I'));
            b.add_exact_receiver_site(base + 1, 8);
            b.add_nested_guarded_receiver_site(base + 1);
        }
        if k_code.get(1) == Some(&0xb8) {
            // `k`'s own call (`h2`), at its pc 1.
            tables
                .invoke_info
                .insert(k_base + 1, (call_row("h2"), 1usize, b'I'));
        }
        if k_code.first() == Some(&0xb2) {
            tables.static_field_info.insert(k_base, (9, 0, b'L', false));
            b.set_static_final_exact_classes(HashMap::from([((9, 0), 10)]));
        }
        b.apply_inline_tables(tables);
        b.build(&combined, code_len)
    })
}

/// A bare open splice frame for the predicate tests (the shape of
/// `r23w1_lambda_forwarder_tests::frame`).
fn test_frame(base: usize, end: usize, invoke_pc: usize) -> SpliceFrame {
    SpliceFrame {
        return_pc: invoke_pc + 3,
        end,
        saved_locals: Vec::new(),
        saved_stack: Vec::new(),
        returns_value: true,
        return_narrow: None,
        multi_return: false,
        bimorphic_reopen: None,
        exits: Vec::new(),
        exit_bci: invoke_pc,
        ranges: Vec::new(),
        next_range: 1,
        range_end: end,
        ctor_receiver: None,
        receiver: NO_NODE,
        arg0: NO_NODE,
        receiver_exact_class: 0,
        invoke_pc,
        base,
        method_key: "p/Host.k:(I)I".to_string(),
        code_len: end - base,
        max_locals: 2,
        sync_monitor: None,
        miss_call: None,
        frameless: false,
        opaque: false,
        reexec_args: Vec::new(),
    }
}

fn chain_on() -> bool {
    ir_splice_frame_states_enabled() && ir_splice_chain_fences_enabled() && ir_site_trap_enabled()
}

/// With the r24 pure route off, the nested `k` at a covered `invoke` is
/// refused by the builder's backstop unless the switch is on; on, it is
/// spliced and its span joins the covered ranges the lowering checks.
#[test]
fn a_covered_nested_body_is_spliced_and_its_span_is_covered() {
    if !chain_on() {
        return;
    }
    let (base, k_base) = (5usize, 5 + G.len());
    assert!(
        build(&[(PURE, Some("0")), (GUARDED, Some("0"))]).is_none(),
        "off: the backstop refuses the compile"
    );
    let g = build(&[(PURE, Some("0")), (GUARDED, Some("1"))]).expect("on: it builds");
    assert!(g.splice_handler_covered.contains(&(base, base + 7)));
    assert!(
        g.splice_handler_covered
            .contains(&(k_base, k_base + K.len())),
        "the nested span: {:?}",
        g.splice_handler_covered
    );
    assert!(
        !g.nodes
            .iter()
            .any(|n| matches!(n.op, Op::Call { .. }) && n.bytecode_pc == Some(base + 1)),
        "k is spliced, not called"
    );
}

/// A call inside `k` has no table of its own. Its exception pad inherits
/// `g`'s table at the invoke that opened `k`.
#[test]
fn a_call_inside_the_nested_body_inherits_the_enclosing_table() {
    if !chain_on() {
        return;
    }
    let calls: [u8; 5] = [0x1a, 0xb8, 0x00, 0x04, 0xac];
    let g = build_with(&calls, &[(PURE, Some("0")), (GUARDED, Some("1"))])
        .expect("the nested call inherits g's RuntimeException row");
    let nested_pc = 5 + G.len() + 1;
    let site = g
        .call_throw_sites
        .iter()
        .find(|s| s.bci as usize == nested_pc)
        .expect("the nested call has its own exception pad");
    assert_eq!(
        site.candidates,
        vec![("java/lang/RuntimeException", 5 + 11)]
    );
    assert!(site.spliced);
    assert_eq!(site.declaring_class_id, 7);
}

/// The `new` admission needs the switch, a body open at `at`, and only
/// non-`Error` catch types on the whole chain.
#[test]
fn an_allocation_is_admitted_only_under_non_error_rows() {
    let mut b = IrBuilder::new(1, 1);
    let row = |s: usize, e: usize, name: &'static str| IrSpliceHandlerRow {
        start_pc: s,
        end_pc: e,
        handler_pc: e + 1,
        catch_name: name,
        declaring_class_id: 7,
    };
    let ask = |b: &IrBuilder, v: Option<&str>, at: usize| {
        cratonvm_types::flags::with_thread_overrides(&[(NEW_NON_ERROR, v)], || {
            b.splice_handler_new_admitted(at)
        })
    };
    // No open splice: nothing to judge.
    assert!(!ask(&b, Some("1"), 10));
    b.splice_handler_rows
        .insert(5, vec![row(5, 12, "java/lang/IllegalArgumentException")]);
    b.splice.push(test_frame(5, 21, 1));
    assert!(ask(&b, Some("1"), 6), "IAE cannot catch an OOME");
    assert!(ask(&b, None, 6), "default on");
    assert!(!ask(&b, Some("0"), 6), "kill switch");
    assert!(
        !ask(&b, Some("1"), 13),
        "not covered: not this rule's question"
    );
    assert!(!ask(&b, Some("1"), 30), "outside the open body");
    // A nested body opened at 7 (covered by IAE): with no rows of its own,
    // the parent's IAE alone decides; its own Throwable row refuses.
    b.splice.push(test_frame(40, 50, 7));
    assert!(
        ask(&b, Some("1"), 42),
        "no row of its own covers 42: the parent's IAE alone"
    );
    b.splice_handler_rows
        .insert(40, vec![row(40, 45, "java/lang/Throwable")]);
    assert!(!ask(&b, Some("1"), 42), "Throwable catches an Error");
    b.splice_handler_rows
        .insert(40, vec![row(40, 45, "java/lang/RuntimeException")]);
    assert!(ask(&b, Some("1"), 42));
    b.splice_handler_rows
        .insert(5, vec![row(5, 12, "java/lang/Error")]);
    assert!(
        !ask(&b, Some("1"), 42),
        "the parent's row at the invoke catches Error"
    );
}

#[test]
fn inherited_edges_use_the_handler_scopes_locals_and_memory_after_the_call() {
    cratonvm_types::flags::with_thread_overrides(&[(GUARDED, Some("1"))], || {
        let mut b = IrBuilder::new(1, 1);
        let outer = b.iconst(123);
        let inner = b.iconst(456);
        b.locals = vec![inner];
        b.splice.push(test_frame(5, 21, 1));
        let mut child = test_frame(40, 50, 7);
        child.saved_locals = vec![outer];
        b.splice.push(child);
        b.splice_handler_rows.insert(
            5,
            vec![IrSpliceHandlerRow {
                start_pc: 5,
                end_pc: 12,
                handler_pc: 16,
                catch_name: "java/lang/RuntimeException",
                declaring_class_id: 7,
            }],
        );
        b.call_throw_planned.insert(7, vec![16]);
        let call = b.graph.add(
            Op::Call {
                info_ptr: call_row("h"),
            },
            IrType::Int,
            vec![b.ctrl, b.mem, inner],
            Some(42),
        );
        b.mem = call;
        b.plant_call_throw_edges(42, call, 5);
        assert!(b.splice_handler_planted_calls.contains(&call));
        let merge = &b.merges[&16];
        assert_eq!(merge.local_snapshots, vec![vec![outer]]);
        assert_eq!(merge.mem_inputs, vec![call]);
        assert_eq!(
            b.locals,
            vec![inner],
            "normal continuation keeps the inner scope"
        );
        assert_eq!(b.graph.call_throw_sites[0].bci, 42);
        assert_eq!(merge.stack_snapshots[0].len(), 1);
    });
}

#[test]
fn the_planner_admits_a_surviving_call_but_not_an_unresolved_invoke() {
    let mut n = ctor(vec![], vec![]);
    assert!(!admitted(&n));
    n.site.invoke_targets.push((
        1,
        crate::InlineInvokeTarget {
            class_name: "p/Base".to_string(),
            method_name: "h".to_string(),
            descriptor: "()V".to_string(),
            invoke_kind: 3,
            direct_entry: Some((1, false)),
            ..Default::default()
        },
    ));
    assert!(admitted(&n));
}

#[test]
fn a_guarded_nested_rows_miss_call_has_the_enclosing_handler_edges() {
    if !chain_on() {
        return;
    }
    let g = build_options(
        &[0x04, 0xac],
        &[
            (PURE, Some("0")),
            (GUARDED, Some("1")),
            ("CRATONVM_JIT_IR_NESTED_GUARDED_SPLICE", Some("1")),
            ("CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS", Some("0")),
        ],
        true,
    )
    .expect("a covered guarded row builds with an armed miss call");
    assert!(g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::ExactClassIs { class_id: 8 })));
    let call = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Call { .. }) && n.bytecode_pc == Some(6))
        .expect("miss call");
    assert!(g.nodes.iter().any(
        |n| matches!(n.op, Op::CallThrew { .. }) && n.inputs.get(1) == Some(&(call as NodeId))
    ));
    assert!(g.call_throw_sites.iter().any(|s| s.bci == 6 && s.spliced));
}

#[test]
fn a_nested_array_length_is_guarded_with_its_own_chain_snapshot() {
    if !chain_on() {
        return;
    }
    let g = build_options(
        &[0x2a, 0xbe, 0xac],
        &[(PURE, Some("0")), (GUARDED, Some("1")), (PROVEN, Some("1"))],
        false,
    )
    .expect("array length under an inherited catch uses a precise null guard");
    let pc = 5 + G.len() + 1;
    let (id, length) = g
        .nodes
        .iter()
        .enumerate()
        .find(|(_, n)| n.op == Op::ArrayLength && n.bytecode_pc == Some(pc))
        .expect("nested array length");
    let guard = guard_token_of(length);
    assert!(matches!(g.nodes[guard as usize].op, Op::Guard { bci } if bci == pc));
    assert!(g.splice_chains_required);
    assert!(g.splice_handler_proven_exits.contains(&(id as NodeId)));
}

#[test]
fn a_trusted_final_static_read_needs_no_active_holder_frame() {
    if !chain_on() {
        return;
    }
    let g = build_options(
        &[0xb2, 0, 5, 0x57, 0x04, 0xac],
        &[(PURE, Some("0")), (GUARDED, Some("1")), (PROVEN, Some("1"))],
        false,
    )
    .expect("the published final-class fact proves initialization");
    let (id, _) = g
        .nodes
        .iter()
        .enumerate()
        .find(|(_, n)| {
            matches!(
                n.op,
                Op::LoadStatic {
                    class_id: 9,
                    field_index: 0,
                    ..
                }
            )
        })
        .expect("static read");
    assert!(g.splice_handler_proven_exits.contains(&(id as NodeId)));
}

#[test]
fn an_untrapped_throw_is_still_an_unarmed_exit() {
    let mut b = IrBuilder::new(1, 1);
    let mark = b.graph.nodes.len();
    b.graph
        .add(Op::Throw, IrType::Void, vec![b.ctrl, b.mem], Some(6));
    assert!(!b.splice_handler_nodes_armed(mark, 6));
}
