use super::*;

fn info(method_name: &'static str, invoke_kind: u8) -> usize {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "P",
        method_name,
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    info as *const JitInvokeInfo as usize
}

fn site(base: usize, code_len: usize, instance: bool, key: &str) -> IrInlineSite {
    IrInlineSite {
        base,
        code_len,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: instance,
        method_key: key.to_string(),
        class_id: 7,
    }
}

fn calls(g: &Graph, info: usize) -> usize {
    g.nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { info_ptr } if info_ptr == info))
        .count()
}

/// `static int f(P p) { return p.m() + h(p); }` with `m` = `this.g()`
/// (guarded exact site, class 7) and `static int h(P q) { return q.g(); }`
/// spliced after it. Both `g` sites are nested exact rows for class 7.
///
/// ```text
///  0: aload_0  1: invokevirtual m  4: aload_0  5: invokestatic h
///  8: iadd  9: ireturn
/// 10: m: aload_0  11: invokevirtual g  14: ireturn
/// 15: h: aload_0  16: invokevirtual g  19: ireturn
/// 20: g: iconst_5  21: ireturn
/// 22: g: iconst_5  23: ireturn
/// ```
const GUARDED_THEN_HELPER: [u8; 26] = [
    0x2a, 0xb6, 0x00, 0x01, 0x2a, 0xb8, 0x00, 0x02, 0x60, 0xac, // f
    0x2a, 0xb6, 0x00, 0x03, 0xac, // m
    0x2a, 0xb6, 0x00, 0x04, 0xac, // h
    0x08, 0xac, 0x08, 0xac, // the two g bodies
    0x00, 0x00,
];

/// `(m calls, g-in-m calls, g-in-h calls, class tests)`.
fn build_guarded_then_helper() -> (usize, usize, usize, usize) {
    let (m, h, g_in_m, g_in_h) = (info("m", 0), info("h", 3), info("g", 0), info("g", 0));
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    b.set_invoke_info(HashMap::from([
        (1usize, (m, 1usize, b'I')),
        (5usize, (h, 1usize, b'I')),
        (11usize, (g_in_m, 1usize, b'I')),
        (16usize, (g_in_h, 1usize, b'I')),
    ]));
    let mut tables = IrInlineTables::default();
    tables.sites.insert(1, site(10, 5, true, "P.m:()I"));
    tables.sites.insert(5, site(15, 5, false, "P.h:(LP;)I"));
    tables.sites.insert(11, site(20, 2, true, "P.g:()I"));
    tables.sites.insert(16, site(22, 2, true, "P.g:()I"));
    b.apply_inline_tables(tables);
    b.add_exact_receiver_site(1, 7);
    b.add_guarded_receiver_site(1);
    b.add_exact_receiver_site(11, 7);
    b.add_exact_receiver_site(16, 7);
    let g = b.build(&GUARDED_THEN_HELPER, 10).expect("builds");
    let tests = g
        .nodes
        .iter()
        .filter(|n| n.op == Op::ExactClassIs { class_id: 7 })
        .count();
    (calls(&g, m), calls(&g, g_in_m), calls(&g, g_in_h), tests)
}

/// I8-2: `g` on the guarded body's `this` splices with no test of its own;
/// `m`'s call stays as the miss edge. `h`'s `g` is on a parameter that the
/// guarded test says nothing about outside the hit edge: it stays a call.
/// Before this wave `h`'s `g` was spliced too, because the proof inside
/// `m` recorded `p` as a node fact.
#[test]
fn a_guarded_frames_fact_serves_its_own_body_only() {
    let (m_calls, g_in_m, g_in_h, tests) = build_guarded_then_helper();
    assert_eq!(tests, 1, "one class test, for m");
    assert_eq!(m_calls, 1, "m's call is the miss edge");
    assert_eq!(
        g_in_m, 0,
        "g on the guarded this is spliced on the hit edge"
    );
    assert_eq!(g_in_h, 1, "g on the parameter, after the join, is not");
}

