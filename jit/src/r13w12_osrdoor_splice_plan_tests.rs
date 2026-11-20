use super::*;

fn leaf(class: &str, method: &str, desc: &str, code_len: usize) -> InlineSite {
    InlineSite {
        callee_code: vec![0; code_len.saturating_add(2)],
        callee_code_len: code_len,
        callee_max_locals: 1,
        callee_num_args: 1,
        callee_is_static: true,
        return_type: b'I',
        class_name: class.to_string(),
        method_name: method.to_string(),
        descriptor: desc.to_string(),
        ..InlineSite::default()
    }
}

// 0 iload_0; 1 invokestatic #2; 4 invokespecial #3; 7 invokevirtual #4;
// 10 goto 1; 13 invokestatic #5 (after the loop); 16 ireturn; plus the
// two padding bytes the door's bytecode carries. `CODE_LEN` is 17.
const CODE: [u8; 19] = [
    0x1a, 0xb8, 0, 2, 0xb7, 0, 3, 0xb6, 0, 4, 0xa7, 0xff, 0xf7, 0xb8, 0, 5, 0xac, 0, 0,
];
const CODE_LEN: usize = 17;

fn candidate(pc: usize, invoke_kind: u8, method: &'static str) -> OsrSpliceCandidate<'static> {
    OsrSpliceCandidate {
        pc,
        invoke_kind,
        class_name: "p/Helper",
        method_name: method,
        descriptor: "(I)I",
    }
}

#[test]
fn a_static_leaf_is_planned_with_its_dependency() {
    let resolve = |c: &str, m: &str, d: &str| Some(leaf(c, m, d, 4));
    let plan = plan_osr_door_splices(
        &CODE,
        CODE_LEN,
        ("p/Main", "main", "([Ljava/lang/String;)V"),
        &[],
        false,
        None,
        None,
        &[candidate(1, 3, "ref")],
        &resolve,
    );
    assert!(plan.inline_sites.contains_key(&1));
    assert_eq!(
        plan.inlined_methods,
        vec![(
            "p/Helper".to_string(),
            "ref".to_string(),
            "(I)I".to_string()
        )]
    );
    assert_eq!(plan.tally.inlined_sites, 1);
}

/// Round nio20 wave 3 (lane L3, W2L2-3): under precise exception frames
/// the door no longer clears its plan; it plans a static body that cannot
/// throw, refuses every other body as `PreciseExceptionFrames`, and refuses
/// an `invokespecial` (receiver null check) before asking the resolver.
#[test]
fn precise_exception_frames_plan_only_a_static_body_that_cannot_throw() {
    // A `try` around the whole loop (and the code after it).
    let table = [cratonvm_reader::attribute::ExceptionTableEntry {
        start_pc: 0,
        end_pc: 16,
        handler_pc: 16,
        catch_type: 0,
    }];
    let asked = std::cell::Cell::new(0u32);
    // `leaf`'s body is `nop`s with no return: not a body that cannot throw.
    let resolve = |c: &str, m: &str, d: &str| {
        asked.set(asked.get() + 1);
        Some(leaf(c, m, d, 4))
    };
    let plan = plan_osr_door_splices(
        &CODE,
        CODE_LEN,
        ("p/Main", "main", "()V"),
        &table,
        true,
        None,
        None,
        &[candidate(1, 3, "ref"), candidate(4, 1, "init")],
        &resolve,
    );
    assert!(plan.inline_sites.is_empty());
    if osr_splice_precise_nothrow_enabled() {
        // The static site was resolved and refused; the special one never
        // reached the resolver.
        assert_eq!(asked.get(), 1);
        assert_eq!(plan.tally.refusal_count("precise-exception-frames"), 2);
    } else {
        assert_eq!(asked.get(), 0);
    }

    // The same method whose `try` covers none of the loop: the table does
    // not concern these sites, which plan as without one.
    let elsewhere = [cratonvm_reader::attribute::ExceptionTableEntry {
        start_pc: 13,
        end_pc: 16,
        handler_pc: 16,
        catch_type: 0,
    }];
    let plan = plan_osr_door_splices(
        &CODE,
        CODE_LEN,
        ("p/Main", "main", "()V"),
        &elsewhere,
        true,
        None,
        None,
        &[candidate(1, 3, "ref"), candidate(4, 1, "init")],
        &resolve,
    );
    let mut pcs: Vec<usize> = plan.inline_sites.keys().copied().collect();
    pcs.sort_unstable();
    if osr_splice_precise_nothrow_enabled() {
        assert_eq!(pcs, vec![1, 4]);
    } else {
        assert!(pcs.is_empty());
    }

    // `Reference.reachabilityFence(Object)`'s shape: an empty static body.
    let empty = |c: &str, m: &str, d: &str| {
        Some(InlineSite {
            callee_code: vec![0xb1, 0, 0],
            callee_code_len: 1,
            callee_max_locals: 1,
            callee_num_args: 1,
            callee_is_static: true,
            return_type: b'V',
            class_name: c.to_string(),
            method_name: m.to_string(),
            descriptor: d.to_string(),
            ..InlineSite::default()
        })
    };
    let plan = plan_osr_door_splices(
        &CODE,
        CODE_LEN,
        ("p/Main", "main", "()V"),
        &table,
        true,
        None,
        Some(1),
        &[candidate(1, 3, "reachabilityFence")],
        &empty,
    );
    assert_eq!(
        plan.inline_sites.contains_key(&1),
        osr_splice_precise_nothrow_enabled()
    );
}

/// Round nio20 wave 4 (lane L3, W3L3-1): the method-entry door's per-site
/// interlock. Without precise frames it never holds; with them it holds at
/// a protected site unless the site is an `invokestatic` of a body that
/// cannot throw, and never at an unprotected site (switch on).
#[test]
fn the_entry_interlock_is_answered_per_site() {
    let table = [cratonvm_reader::attribute::ExceptionTableEntry {
        start_pc: 0,
        end_pc: 10,
        handler_pc: 16,
        catch_type: 0,
    }];
    let nothrow = InlineSite {
        callee_code: vec![0x1a, 0xac, 0, 0],
        callee_code_len: 2,
        callee_max_locals: 1,
        callee_num_args: 1,
        callee_is_static: true,
        return_type: b'I',
        ..InlineSite::default()
    };
    let throws = InlineSite {
        callee_code: vec![0x1a, 0x1a, 0x6c, 0xac, 0, 0],
        callee_code_len: 4,
        ..nothrow.clone()
    };
    // No precise frames: never.
    assert!(!sp_entry_precise_interlock_holds(
        false,
        &table,
        1,
        3,
        Some(&throws)
    ));
    assert!(!sp_entry_precise_interlock_holds(false, &table, 1, 0, None));
    let per_site = sp_splice_precise_per_site_enabled();
    // Protected: a static body that cannot throw only.
    assert_eq!(
        sp_entry_precise_interlock_holds(true, &table, 1, 3, Some(&nothrow)),
        !per_site
    );
    assert!(sp_entry_precise_interlock_holds(
        true,
        &table,
        1,
        3,
        Some(&throws)
    ));
    assert!(sp_entry_precise_interlock_holds(
        true,
        &table,
        1,
        1,
        Some(&nothrow)
    ));
    assert!(sp_entry_precise_interlock_holds(true, &table, 1, 0, None));
    // Unprotected (pc 13): every kind, every body.
    assert_eq!(
        sp_entry_precise_interlock_holds(true, &table, 13, 3, Some(&throws)),
        !per_site
    );
    assert_eq!(
        sp_entry_precise_interlock_holds(true, &table, 13, 0, None),
        !per_site
    );
}

/// W3L3-2: the guarded OSR pass plans an unprotected virtual site of a
/// method with a table and refuses a protected one.
#[test]
fn guarded_pass_plans_only_unprotected_sites_under_precise_frames() {
    if !osr_door_guarded_splices_enabled() {
        return;
    }
    let mut receivers = profile::ReceiverCounts::default();
    receivers.insert(7, 5_000);
    let mut profile = profile::MethodProfile::default();
    profile.receivers.insert(7, receivers);
    let namer = |_cid: u32| Some("p/Impl".to_string());
    let receiver_resolve = |_cid: u32, _c: &str, m: &str, d: &str| {
        Some(InlineSite {
            callee_is_static: false,
            callee_num_args: 2,
            ..leaf("p/Impl", m, d, 4)
        })
    };
    let run = |table: &[cratonvm_reader::attribute::ExceptionTableEntry]| {
        let mut plan = OsrSplicePlan::default();
        plan_osr_door_guarded_splices(
            &mut plan,
            &CODE,
            CODE_LEN,
            ("p/Main", "main", "()V"),
            table,
            true,
            Some(&profile),
            Some(1),
            &[candidate(7, 0, "virt")],
            &namer,
            &receiver_resolve,
        );
        plan
    };
    // The `try` covers the virtual site at pc 7.
    let covering = [cratonvm_reader::attribute::ExceptionTableEntry {
        start_pc: 0,
        end_pc: 13,
        handler_pc: 16,
        catch_type: 0,
    }];
    let plan = run(&covering[..]);
    assert!(plan.guard_variants.is_empty());
    // The `try` covers only the code after the loop.
    let elsewhere = [cratonvm_reader::attribute::ExceptionTableEntry {
        start_pc: 13,
        end_pc: 16,
        handler_pc: 16,
        catch_type: 0,
    }];
    let plan = run(&elsewhere[..]);
    if sp_splice_precise_per_site_enabled() {
        // Admitted exactly as without a table would be; whether the
        // policy admits this synthetic monomorphic profile is
        // `plan_inline`'s business, so compare against the no-table run.
        let mut bare = OsrSplicePlan::default();
        plan_osr_door_guarded_splices(
            &mut bare,
            &CODE,
            CODE_LEN,
            ("p/Main", "main", "()V"),
            &[],
            false,
            Some(&profile),
            Some(1),
            &[candidate(7, 0, "virt")],
            &namer,
            &receiver_resolve,
        );
        assert_eq!(
            plan.guard_variants.contains_key(&7),
            bare.guard_variants.contains_key(&7)
        );
    } else {
        assert!(plan.guard_variants.is_empty());
    }
}

/// W2L2-3: the bytecode screen admits local / constant / arithmetic /
/// forward-branch / return bodies and nothing that can throw.
#[test]
fn only_bodies_that_cannot_throw_pass_the_bytecode_screen() {
    // return; iload_0 ireturn; a forward ifeq diamond.
    assert!(bytecode_body_cannot_throw(&[0xb1]));
    assert!(bytecode_body_cannot_throw(&[0x1a, 0xac]));
    assert!(bytecode_body_cannot_throw(&[
        0x1a, 0x99, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac
    ]));
    // iadd, lmul, fdiv, ddiv, frem: no exception.
    assert!(bytecode_body_cannot_throw(&[0x1a, 0x1b, 0x60, 0xac]));
    assert!(bytecode_body_cannot_throw(&[0x22, 0x23, 0x6e, 0xae]));
    // idiv, irem, ldiv, lrem: ArithmeticException.
    for op in [0x6c, 0x70] {
        assert!(!bytecode_body_cannot_throw(&[0x1a, 0x1b, op, 0xac]));
    }
    for op in [0x6d, 0x71] {
        assert!(!bytecode_body_cannot_throw(&[0x1e, 0x20, op, 0xad]));
    }
    // getfield, iaload, invokestatic, new, athrow, checkcast, monitorenter.
    assert!(!bytecode_body_cannot_throw(&[0x2a, 0xb4, 0, 1, 0xac]));
    assert!(!bytecode_body_cannot_throw(&[0x2a, 0x03, 0x2e, 0xac]));
    assert!(!bytecode_body_cannot_throw(&[0xb8, 0, 1, 0xb1]));
    assert!(!bytecode_body_cannot_throw(&[0xbb, 0, 1, 0xb0]));
    assert!(!bytecode_body_cannot_throw(&[0x2a, 0xbf]));
    assert!(!bytecode_body_cannot_throw(&[0x2a, 0xc0, 0, 1, 0xb0]));
    assert!(!bytecode_body_cannot_throw(&[0x2a, 0xc2, 0xb1]));
    // ldc (may resolve and allocate), a backward branch, a branch past
    // the body, no return, a truncated operand.
    assert!(!bytecode_body_cannot_throw(&[0x12, 1, 0xb0]));
    assert!(!bytecode_body_cannot_throw(&[0x03, 0x99, 0xff, 0xff, 0xb1]));
    assert!(!bytecode_body_cannot_throw(&[0x03, 0x99, 0x00, 0x09, 0xb1]));
    assert!(!bytecode_body_cannot_throw(&[0x00]));
    assert!(!bytecode_body_cannot_throw(&[0x11, 0]));
    assert!(!bytecode_body_cannot_throw(&[]));
}

/// W2L2-3: the site-level screen also wants a static, unsynchronized
/// callee whose resolver output names nothing it touches.
#[test]
fn the_site_screen_wants_a_static_body_that_touches_nothing() {
    let base = InlineSite {
        callee_code: vec![0x1a, 0xac, 0, 0],
        callee_code_len: 2,
        callee_max_locals: 1,
        callee_num_args: 1,
        callee_is_static: true,
        return_type: b'I',
        ..InlineSite::default()
    };
    assert!(inline_site_body_cannot_throw(&base));
    let instance = InlineSite {
        callee_is_static: false,
        ..base.clone()
    };
    assert!(!inline_site_body_cannot_throw(&instance));
    let synchronized = InlineSite {
        callee_is_synchronized: true,
        ..base.clone()
    };
    assert!(!inline_site_body_cannot_throw(&synchronized));
    let field = InlineSite {
        field_info: vec![(0, 0, b'I')],
        ..base.clone()
    };
    assert!(!inline_site_body_cannot_throw(&field));
    let masked = InlineSite {
        callee_original_code: Some(vec![0x1a, 0xac]),
        callee_original_handler_rows: Vec::new(),
        ..base.clone()
    };
    assert!(!inline_site_body_cannot_throw(&masked));
    let overlong = InlineSite {
        callee_code_len: 9,
        ..base
    };
    assert!(!inline_site_body_cannot_throw(&overlong));
}

/// Round nio20 wave 3 (lane L3, L3W2-6): the overdraft is 1333 budget
/// units, and only a site inside a hot loop of a plan under the hot-loop
/// site cap may draw on it.
#[test]
fn the_hot_loop_overdraft_serves_hot_loop_sites_under_the_cap() {
    assert_eq!(OSR_DOOR_HOT_LOOP_OVERDRAFT, 3333);
    let hot = [(1usize, 10usize)];
    let mut plan = OsrSplicePlan {
        hot_loop_overdraft_left: 100,
        ..OsrSplicePlan::default()
    };
    assert_eq!(osr_door_overdraft_for(&plan, &hot, 4), 100);
    assert_eq!(osr_door_overdraft_for(&plan, &hot, 13), 0);
    assert_eq!(osr_door_overdraft_for(&plan, &[], 4), 0);
    for pc in 100..100 + ir_splice_order::IR_INLINE_MAX_SITES_HOT_LOOP {
        plan.inline_sites.insert(pc, InlineSite::default());
    }
    assert_eq!(osr_door_overdraft_for(&plan, &hot, 4), 0);
}


#[test]
fn an_unresolved_callee_is_counted_and_left_to_its_call() {
    let resolve = |_: &str, _: &str, _: &str| -> Option<InlineSite> { None };
    let plan = plan_osr_door_splices(
        &CODE,
        CODE_LEN,
        ("p/Main", "main", "()V"),
        &[],
        false,
        None,
        None,
        &[candidate(1, 3, "ref")],
        &resolve,
    );
    assert!(plan.inline_sites.is_empty());
    assert_eq!(plan.tally.refusal_count("callee-unresolved"), 1);
}

/// Round 14 wave 3 (lane calls, OD-1): the loop the OSR trigger fired in is
/// found from its header, its back edge, or a pc inside it; a pc after the
/// loop is in none.
#[test]
fn the_osr_entry_loop_is_found_from_its_header_back_edge_or_body() {
    assert_eq!(osr_entry_loop_ranges(&CODE, CODE_LEN, 1), vec![(1, 10)]);
    assert_eq!(osr_entry_loop_ranges(&CODE, CODE_LEN, 10), vec![(1, 10)]);
    assert_eq!(osr_entry_loop_ranges(&CODE, CODE_LEN, 4), vec![(1, 10)]);
    assert!(osr_entry_loop_ranges(&CODE, CODE_LEN, 13).is_empty());
}

/// OD-1: with no profile, a 60-byte in-loop leaf is over the cold
/// `MaxInlineSize` (35) and stays a call; the OSR entry's loop is hot
/// evidence and prices it at `FreqInlineSize`, so it is planned.
#[test]
fn the_osr_entry_loop_prices_an_unprofiled_site_hot() {
    let resolve = |c: &str, m: &str, d: &str| Some(leaf(c, m, d, 60));
    let plan_for = |entry: Option<usize>| {
        plan_osr_door_splices(
            &CODE,
            CODE_LEN,
            ("p/Main", "main", "()V"),
            &[],
            false,
            None,
            entry,
            &[candidate(1, 3, "ref")],
            &resolve,
        )
    };
    assert!(plan_for(None).inline_sites.is_empty());
    {
        assert!(plan_for(Some(1)).inline_sites.contains_key(&1));
    }
}
