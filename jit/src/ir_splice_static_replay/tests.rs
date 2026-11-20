use super::*;
use crate::ir::{IrBuilder, IrInlineTables, Op};

const S: StaticRow = (7, 0, b'I', false);

fn rows(list: &[(usize, StaticRow)]) -> impl Fn(usize) -> Option<StaticRow> + '_ {
    move |pc| list.iter().find(|r| r.0 == pc).map(|r| r.1)
}

fn no_volatile(_: usize) -> bool {
    false
}

/// `static int g(int n) { for (int i = 0; i < n; i++) S = i; return 0; }`
/// (javac shape; `putstatic` at 8).
const BLIND_LOOP: [u8; 19] = [
    0x03, 0x3c, // 0 iconst_0; 1 istore_1
    0x1b, 0x1a, // 2 iload_1; 3 iload_0
    0xa2, 0x00, 0x0d, // 4 if_icmpge 17
    0x1b, // 7 iload_1
    0xb3, 0x00, 0x02, // 8 putstatic S
    0x84, 0x01, 0x01, // 11 iinc 1, 1
    0xa7, 0xff, 0xf4, // 14 goto 2
    0x03, 0xac, // 17 iconst_0; 18 ireturn
];

#[test]
fn a_loop_storing_only_a_local_into_a_primitive_static_replays() {
    assert!(body_replays_idempotently(&BLIND_LOOP, rows(&[(8, S)]), no_volatile));
}

#[test]
fn a_body_reading_the_static_it_writes_does_not() {
    // `S = S + 1`: 0 getstatic S; 3 iconst_1; 4 iadd; 5 putstatic S;
    // 8 return
    let body = [0xb2, 0x00, 0x02, 0x04, 0x60, 0xb3, 0x00, 0x02, 0xb1];
    assert!(!body_replays_idempotently(&body, rows(&[(0, S), (5, S)]), no_volatile));
    // Another field of the same (initialised) class may be read...
    let other_field = (7, 1, b'I', false);
    assert!(body_replays_idempotently(
        &body,
        rows(&[(0, other_field), (5, S)]),
        no_volatile
    ));
    // ...but not one of a class it does not write (its `<clinit>`).
    let other_class = (8, 0, b'I', false);
    assert!(!body_replays_idempotently(
        &body,
        rows(&[(0, other_class), (5, S)]),
        no_volatile
    ));
    // Nor a volatile one.
    let volatile = (7, 1, b'I', true);
    assert!(!body_replays_idempotently(&body, rows(&[(0, volatile), (5, S)]), no_volatile));
}

#[test]
fn a_reference_volatile_rowless_or_classless_store_does_not() {
    for row in [
        (7, 0, b'L', false),
        (7, 0, b'[', false),
        (7, 0, b'I', true),
        (0, 0, b'I', false),
    ] {
        assert!(
            !body_replays_idempotently(&BLIND_LOOP, rows(&[(8, row)]), no_volatile),
            "{row:?}"
        );
    }
    assert!(!body_replays_idempotently(&BLIND_LOOP, rows(&[]), no_volatile));
}

#[test]
fn every_other_write_call_monitor_or_throw_refuses() {
    // Replace the `iinc` at 11 (3 bytes) by each 3-byte-or-shorter
    // offender, padded with `nop`s so the branch offsets stay valid.
    for offender in [
        &[0x4f, 0x00, 0x00][..], // iastore
        &[0xb5, 0x00, 0x03],     // putfield
        &[0xb8, 0x00, 0x04],     // invokestatic
        &[0xb6, 0x00, 0x04],     // invokevirtual
        &[0xbb, 0x00, 0x05],     // new
        &[0xbc, 0x0a, 0x00],     // newarray
        &[0xc2, 0x00, 0x00],     // monitorenter
        &[0xbf, 0x00, 0x00],     // athrow
        &[0xc0, 0x00, 0x06],     // checkcast
    ] {
        let mut body = BLIND_LOOP;
        body[11..14].copy_from_slice(offender);
        assert!(
            !body_replays_idempotently(&body, rows(&[(8, S)]), no_volatile),
            "{offender:02x?}"
        );
    }
}

#[test]
fn a_volatile_getfield_refuses_and_a_plain_one_replays() {
    // 0 aload_0; 1 getfield #3; 4 putstatic S; 7 return
    let body = [0x2a, 0xb4, 0x00, 0x03, 0xb3, 0x00, 0x02, 0xb1];
    assert!(body_replays_idempotently(&body, rows(&[(4, S)]), no_volatile));
    assert!(!body_replays_idempotently(&body, rows(&[(4, S)]), |pc| pc == 1));
}

#[test]
fn wide_local_forms_replay_and_wide_ret_does_not() {
    // 0 wide iload 1; 4 putstatic S; 7 wide iinc 1 1; 13 return
    let body = [
        0xc4, 0x15, 0x00, 0x01, 0xb3, 0x00, 0x02, 0xc4, 0x84, 0x00, 0x01, 0x00, 0x01, 0xb1,
    ];
    assert!(body_replays_idempotently(&body, rows(&[(4, S)]), no_volatile));
    let mut ret = body;
    ret[1] = 0xa9; // wide ret 1
    assert!(!body_replays_idempotently(&ret, rows(&[(4, S)]), no_volatile));
}

/// `static void fillFrom(int[] a, int n) { for (int i = 0; i < n; i++) S = a[i]; }`
/// (`putstatic` at 10): a store, then a trap point (the back edge's poll,
/// the next `iaload`), then a fresh read of the shared array.
const FILL_FROM: [u8; 20] = [
    0x03, 0x3d, // 0 iconst_0; 1 istore_2
    0x1c, 0x1b, // 2 iload_2; 3 iload_1
    0xa2, 0x00, 0x0f, // 4 if_icmpge 19
    0x2a, 0x1c, 0x2e, // 7 aload_0; 8 iload_2; 9 iaload
    0xb3, 0x00, 0x02, // 10 putstatic S
    0x84, 0x02, 0x01, // 13 iinc 2, 1
    0xa7, 0xff, 0xf2, // 16 goto 2
    0xb1, // 19 return
];

/// `static void sumInto(int[] a) { int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; S = s; }`
/// (`putstatic` at 23): every shared read precedes the only store.
const SUM_INTO: [u8; 27] = [
    0x03, 0x3c, 0x03, 0x3d, // 0 iconst_0; istore_1; iconst_0; istore_2
    0x1c, 0x2a, 0xbe, // 4 iload_2; 5 aload_0; 6 arraylength
    0xa2, 0x00, 0x0f, // 7 if_icmpge 22
    0x1b, 0x2a, 0x1c, 0x2e, 0x60, 0x3c, // 10 iload_1 .. 15 istore_1
    0x84, 0x02, 0x01, // 16 iinc 2, 1
    0xa7, 0xff, 0xf1, // 19 goto 4
    0x1b, // 22 iload_1
    0xb3, 0x00, 0x02, // 23 putstatic S
    0xb1, // 26 return
];

/// j27 wave 9 (lane W9RPL): another thread may rewrite `a` between the
/// first run and the replay, so a replay after a published store could
/// publish a value no single run wrote. A store that a trap point can
/// follow refuses once the body reads shared memory.
#[test]
fn a_shared_read_with_a_trap_after_a_store_refuses() {
    assert!(!body_replays_idempotently(&FILL_FROM, rows(&[(10, S)]), no_volatile));
    // The same store loop over locals only keeps replaying.
    assert!(body_replays_idempotently(&BLIND_LOOP, rows(&[(8, S)]), no_volatile));
}

#[test]
fn shared_reads_all_before_the_only_store_replay() {
    assert!(body_replays_idempotently(&SUM_INTO, rows(&[(23, S)]), no_volatile));
}

#[test]
fn a_trapping_division_after_the_store_refuses_only_with_a_shared_read() {
    // 0 aload_0; 1 getfield #3; 4 putstatic S; 7 iload_1; 8 iconst_1;
    // 9 idiv; 10 pop; 11 return
    let body = [0x2a, 0xb4, 0x00, 0x03, 0xb3, 0x00, 0x02, 0x1b, 0x04, 0x6c, 0x57, 0xb1];
    assert!(!body_replays_idempotently(&body, rows(&[(4, S)]), no_volatile));
    // 0 iload_0; 1 nop; 2 nop; 3 nop; then the same tail: no shared read.
    let mut local = body;
    local[..4].copy_from_slice(&[0x1a, 0x00, 0x00, 0x00]);
    assert!(body_replays_idempotently(&local, rows(&[(4, S)]), no_volatile));
    // A getstatic of another field of the written class is shared too.
    // 0 getstatic T; 3 nop; then the same tail.
    let mut stat = body;
    stat[..4].copy_from_slice(&[0xb2, 0x00, 0x04, 0x00]);
    let t = (7, 1, b'I', false);
    assert!(!body_replays_idempotently(&stat, rows(&[(0, t), (4, S)]), no_volatile));
}

#[test]
fn a_body_with_no_store_is_not_this_route() {
    // 0 iconst_0; 1 ireturn
    assert!(!body_replays_idempotently(&[0x03, 0xac], rows(&[]), no_volatile));
}

fn site(base: usize, code_len: usize) -> IrInlineSite {
    IrInlineSite {
        base,
        code_len,
        num_args: 1,
        max_locals: 2,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "P.g:(I)I".to_string(),
        class_id: 7,
    }
}

#[test]
fn only_a_caller_pc_outside_every_appended_body_is_top_level() {
    let sites = HashMap::from([(1usize, site(5, 19))]);
    assert!(caller_pc_is_top_level(&sites, 3), "the method's own code");
    assert!(!caller_pc_is_top_level(&sites, 5 + 7), "inside the body");
    assert!(caller_pc_is_top_level(&sites, 24), "past it");
    assert!(
        !caller_pc_is_top_level(&sites, crate::ir::bimorphic_second_site_key(9)),
        "a tagged key is read as its caller pc"
    );
}


/// `static int f(int n) { return g(n); }` with [`BLIND_LOOP`] spliced at
/// the `invokestatic` (pc 1), class 7 static S at combined pc 13.
fn build_blind_loop_splice(switch_on: bool) -> Option<crate::ir::Graph> {
    let mut code = vec![0x1a, 0xb8, 0x00, 0x01, 0xac]; // caller, code_len 5
    code.extend_from_slice(&BLIND_LOOP);
    code.extend_from_slice(&[0, 0]);
    cratonvm_types::flags::with_thread_overrides(
        &[(SWITCH, Some(if switch_on { "1" } else { "0" }))],
        || {
            let mut b = IrBuilder::new(1, 1);
            let mut tables = IrInlineTables::default();
            tables.sites.insert(1, site(5, BLIND_LOOP.len()));
            tables.static_field_info.insert(5 + 8, S);
            b.apply_inline_tables(tables);
            b.build(&code, 5)
        },
    )
}

#[test]
fn the_builder_splices_the_blind_store_loop_only_with_the_switch() {
    let g = build_blind_loop_splice(true)
        .expect("an idempotent store loop builds without the replay fence");
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| matches!(n.op, Op::StoreStatic { class_id: 7, .. }))
            .count(),
        1
    );
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
        "the call is gone"
    );
    assert!(
        build_blind_loop_splice(false).is_none(),
        "off: the committed store fences the back edge (the planner refuses \
         this body as `ir-splice-putstatic-loop`)"
    );
}
