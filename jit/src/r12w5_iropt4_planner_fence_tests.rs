use super::*;

const ALL: ir::SpliceFenceRelax = ir::SpliceFenceRelax {
    forward_branch: true,
    null_facts: true,
    guarded_call: true,
};

fn target(method: &str, desc: &str, kind: u8) -> InlineInvokeTarget {
    InlineInvokeTarget {
        class_name: "java/util/HashMap".to_string(),
        method_name: method.to_string(),
        descriptor: desc.to_string(),
        num_jit_args: 0,
        return_type: desc.as_bytes().last().copied().unwrap_or(b'V'),
        invoke_kind: kind,
        declaring_class_id: 7,
        ..Default::default()
    }
}

fn body(
    code: Vec<u8>,
    is_static: bool,
    max_locals: usize,
    descriptor: &str,
    invoke_targets: Vec<(usize, InlineInvokeTarget)>,
) -> InlineSite {
    InlineSite {
        callee_code_len: code.len(),
        callee_code: code,
        callee_max_locals: max_locals,
        callee_is_static: is_static,
        class_name: "java/util/HashMap".to_string(),
        class_id: 7,
        method_name: "m".to_string(),
        descriptor: descriptor.to_string(),
        invoke_targets,
        ..Default::default()
    }
}

/// `V get(Object key) { Node e; return (e = getNode(key)) == null ? null : e.value; }`
///
/// ```text
///  0: aload_0  1: aload_1  2: invokevirtual getNode  5: dup  6: astore_2
///  7: ifnonnull -> 14  10: aconst_null  11: goto -> 18
/// 14: aload_2  15: getfield value  18: areturn
/// ```
fn get_body(test: u8) -> InlineSite {
    body(
        vec![
            0x2a, 0x2b, 0xb6, 0x00, 0x05, 0x59, 0x4d, test, 0x00, 0x07, 0x01, 0xa7, 0x00, 0x07,
            0x2c, 0xb4, 0x00, 0x06, 0xb0,
        ],
        false,
        3,
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        vec![(
            2,
            target("getNode", "(Ljava/lang/Object;)Ljava/util/HashMap$Node;", 1),
        )],
    )
}

#[test]
fn get_is_admitted_only_with_the_branch_and_null_fact_relaxations() {
    let get = get_body(0xc7);
    assert!(
        ir_splice_body_may_trap_after_a_call(&get, None),
        "the round-11 rule refuses get"
    );
    assert!(ir_splice_tree_admits_fence_relaxation(&get));
    assert_eq!(ir_splice_relaxed_fence_admits(&get, None, ALL), Some(true));
    assert!(!ir_splice_fence_refuses_site(&get, None, ALL));
    assert!(ir_splice_fence_refuses_site(
        &get,
        None,
        ir::SpliceFenceRelax::NONE
    ));
    let no_branch = ir::SpliceFenceRelax {
        forward_branch: false,
        ..ALL
    };
    assert_eq!(
        ir_splice_relaxed_fence_admits(&get, None, no_branch),
        Some(false)
    );
    let no_facts = ir::SpliceFenceRelax {
        null_facts: false,
        ..ALL
    };
    assert_eq!(
        ir_splice_relaxed_fence_admits(&get, None, no_facts),
        Some(false)
    );
}

/// The must-differ twin: `ifnull`, so `e.value` is read where `e` is null.
#[test]
fn a_read_on_the_null_edge_is_refused() {
    let twin = get_body(0xc6);
    assert_eq!(
        ir_splice_relaxed_fence_admits(&twin, None, ALL),
        Some(false)
    );
    assert!(ir_splice_fence_refuses_site(&twin, None, ALL));
}

/// `static int hash(Object key) { int h; return key == null ? 0 : (h =
/// key.hashCode()) ^ (h >>> 16); }` -- a nested splice that leaves the
/// `hashCode` call behind.
fn hash_body() -> InlineSite {
    body(
        vec![
            0x2a, 0xc7, 0x00, 0x07, 0x03, 0xa7, 0x00, 0x0e, 0x2a, 0xb6, 0x00, 0x09, 0x59, 0x3c,
            0x1b, 0x10, 0x10, 0x7c, 0x82, 0xac,
        ],
        true,
        2,
        "(Ljava/lang/Object;)I",
        vec![(9, target("hashCode", "()I", 0))],
    )
}

/// `V put(K key, V value) { return putVal(hash(key), key, value, false, true); }`
///
/// ```text
///  0: aload_0  1: aload_1  2: invokestatic hash  5: aload_1  6: aload_2
///  7: iconst_0  8: iconst_1  9: invokevirtual putVal  12: areturn
/// ```
fn put_body(receiver_load: u8) -> InlineSite {
    let mut put = body(
        vec![
            receiver_load,
            0x2b,
            0xb8,
            0x00,
            0x07,
            0x2b,
            0x2c,
            0x03,
            0x04,
            0xb6,
            0x00,
            0x08,
            0xb0,
        ],
        false,
        3,
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        vec![
            (2, target("hash", "(Ljava/lang/Object;)I", 3)),
            (
                9,
                target(
                    "putVal",
                    "(ILjava/lang/Object;Ljava/lang/Object;ZZ)Ljava/lang/Object;",
                    1,
                ),
            ),
        ],
    );
    put.nested_sites = vec![NestedInlineSite {
        callee_pc: 2,
        guard_class_id: 0,
        site: hash_body(),
        bimorphic_second: None,
    }].into();
    put
}

#[test]
fn put_is_admitted_with_the_guarded_call_relaxation() {
    let put = put_body(0x2a);
    assert!(
        ir_splice_body_may_trap_after_a_call(&put, None),
        "the round-11 rule refuses put"
    );
    assert!(ir_splice_tree_admits_fence_relaxation(&put));
    assert_eq!(ir_splice_relaxed_fence_admits(&put, None, ALL), Some(true));
    let no_call = ir::SpliceFenceRelax {
        guarded_call: false,
        ..ALL
    };
    assert_eq!(
        ir_splice_relaxed_fence_admits(&put, None, no_call),
        Some(false)
    );
    // The nested `hash` body alone: its own call is its last trap-free tail.
    assert!(!ir_splice_body_may_trap_after_a_call(&hash_body(), None));
}

/// The must-differ twin: `putVal`'s receiver is `key` (`aload_1`), which
/// may be null, not the splice's own receiver.
#[test]
fn a_call_on_an_unproven_receiver_is_refused() {
    let twin = put_body(0x2b);
    assert_eq!(
        ir_splice_relaxed_fence_admits(&twin, None, ALL),
        Some(false)
    );
}

/// `static int f(Object a) { Object v = next(a); if (v != null) {} return
/// v.f; }`: the `ifnull` fall-through fact must end at the join.
#[test]
fn a_null_fact_ends_at_a_join() {
    let site = body(
        vec![
            0x2a, 0xb8, 0x00, 0x02, 0x4b, 0x2a, 0xc6, 0x00, 0x04, 0x00, 0x2a, 0xb4, 0x00, 0x03,
            0xac,
        ],
        true,
        1,
        "(Ljava/lang/Object;)I",
        vec![(
            1,
            target("next", "(Ljava/lang/Object;)Ljava/lang/Object;", 3),
        )],
    );
    assert_eq!(
        ir_splice_relaxed_fence_admits(&site, None, ALL),
        Some(false)
    );
}

/// A body with a back edge keeps the round-11 answer, and so does any
/// tree with a loop or a field write anywhere in it.
#[test]
fn loops_and_field_writes_keep_the_round_11_rule() {
    // inc(); while (i > 0) i--; return i;
    let looped = body(
        vec![
            0xb8, 0x00, 0x02, 0x1b, 0x9e, 0x00, 0x09, 0x84, 0x00, 0xff, 0xa7, 0xff, 0xf9, 0x1b,
            0xac,
        ],
        true,
        1,
        "(I)I",
        vec![(0, target("inc", "()V", 3))],
    );
    assert_eq!(ir_splice_relaxed_fence_admits(&looped, None, ALL), None);
    assert!(ir_splice_fence_refuses_site(&looped, None, ALL));
    assert!(!ir_splice_tree_admits_fence_relaxation(&looped));
    // `this.f = 1` in a nested body.
    let mut put = put_body(0x2a);
    if let Some(n) = put.nested_sites.first_mut() {
        n.site.callee_code = vec![0x2a, 0x04, 0xb5, 0x00, 0x01, 0x03, 0xac];
        n.site.callee_code_len = 7;
    }
    assert!(!ir_splice_tree_admits_fence_relaxation(&put));
}
