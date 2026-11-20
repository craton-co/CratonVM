use super::*;

/// A local the handler reads, redefined by the protected code AFTER the
/// throwing instruction, must still be live AT that instruction.
///
/// This is `C2Handlers.loopInsideTry`'s shape reduced to its essentials.
/// The block-level exception edge alone reported it dead — the backward
/// walk applied the `istore_1` that follows the call — so the precise
/// exceptional frame recorded `sum` as `Undefined` and the handler
/// returned `-0` instead of `-sum`.
#[test]
fn local_redefined_after_throw_site_stays_live_for_the_handler() {
    // 0: iconst_0
    // 1: istore_1        sum = 0
    // 2: iload_1         <- protected range starts; sum pushed
    // 3: iload_0
    // 4: invokestatic #1 <- THROW SITE
    // 7: iadd
    // 8: istore_1        sum redefined (kills local 1 on the normal path)
    // 9: goto 15
    // 12: astore_2       <- handler: reads local 1
    // 13: iload_1
    // 14: ireturn
    // 15: iload_1
    // 16: ireturn
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1
        0x1a, // 3: iload_0
        0xb8, 0x00, 0x01, // 4: invokestatic
        0x60, // 7: iadd
        0x3c, // 8: istore_1
        0xa7, 0x00, 0x06, // 9: goto +6 -> 15
        0x4d, // 12: astore_2
        0x1b, // 13: iload_1
        0xac, // 14: ireturn
        0x1b, // 15: iload_1
        0xac, // 16: ireturn
    ];
    let handlers = [(2usize, 9usize, 12usize)];

    let (live_with, covered_with) =
        live_locals_per_pc_with_handlers(&code, code.len(), 1, &[], &handlers);
    assert!(covered_with[4], "the throw site must be a covered pc");
    assert!(
        live_with[4] & (1 << 1) != 0,
        "local 1 is read by the handler at pc 13 and an exception at pc 4 \
         reaches that handler, so local 1 must be live at pc 4; the \
         `istore_1` at pc 8 never executes on the exception path. \
         live_at[4] = {:#x}",
        live_with[4]
    );

    // Control: with no handlers modelled the same local is genuinely dead
    // there, so this test is pinning the exception edge and not a
    // tautology.
    let (live_without, _) = live_locals_per_pc_with_handlers(&code, code.len(), 1, &[], &[]);
    assert!(
        live_without[4] & (1 << 1) == 0,
        "without the exception edge local 1 is dead at pc 4 (redefined at \
         pc 8 before its only normal-path read) — if this ever becomes \
         live the test above proves nothing"
    );
}

/// The interference-graph half: whatever is defined inside the protected
/// range must not be allowed to share a register with a local the handler
/// reads.
#[test]
fn handler_read_local_interferes_with_defs_inside_the_protected_range() {
    // Same shape, but local 2 is assigned inside the try so it competes
    // with local 1 (which only the handler reads after the redefinition).
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1
        0x1a, // 3: iload_0
        0xb8, 0x00, 0x01, // 4: invokestatic
        0x3d, // 7: istore_2      def inside the range
        0x3c, // 8: istore_1
        0xa7, 0x00, 0x06, // 9: goto -> 15
        0x4e, // 12: astore_3
        0x1b, // 13: iload_1      handler reads local 1
        0xac, // 14: ireturn
        0x1c, // 15: iload_2
        0xac, // 16: ireturn
    ];
    let handlers = [(2usize, 9usize, 12usize)];
    let mut blocks = build_cfg_with_handlers(&code, code.len(), &handlers);
    for block in &mut blocks {
        compute_gen_kill(&code, block);
    }
    assert!(solve_liveness(&mut blocks, 1));
    let ranges = handler_live_in_ranges(&blocks, &handlers);
    let interference = build_interference(&code, &blocks, 4, &ranges);
    assert!(
        interference[2] & (1 << 1) != 0,
        "local 2 is defined at pc 7, inside the protected range, while \
         local 1 must survive for the handler — they must interfere so the \
         allocator cannot give them the same register (interference[2] = \
         {:#x})",
        interference[2]
    );
}

/// r9: `state` is defined before the `try`, redefined inside it, and read
/// by the `catch`. `tmp` lives and dies BEFORE the `try`.
///
/// ```java
/// int state = 0;
/// int tmp = 5; use(tmp);
/// try { call(); state = 1; return state; }
/// catch (Throwable t) { return state; }
/// ```
///
/// Walking backward, the `istore_1` inside the range used to kill `state`
/// for everything before it — the handler's read reached the protected pcs'
/// ROWS but never the running live set, and never the block's live-in — so
/// `state` was dead across `tmp`'s whole life, the two did not interfere,
/// and a colouring was free to put both in one register. An exception at
/// pc 6 then entered the handler with `tmp`'s 5 in place of `state`'s 0.
const STATE_BEFORE_TRY: [u8; 16] = [
    0x03, // 0: iconst_0
    0x3c, // 1: istore_1        state = 0
    0x08, // 2: iconst_5
    0x3d, // 3: istore_2        tmp = 5
    0x1c, // 4: iload_2
    0x57, // 5: pop             tmp dies here
    0xb8, 0x00, 0x01, // 6: invokestatic   <- protected [6, 11)
    0x04, // 9: iconst_1
    0x3c, // 10: istore_1       state = 1
    0x1b, // 11: iload_1
    0xac, // 12: ireturn
    0x4e, // 13: astore_3       <- handler
    0x1b, // 14: iload_1        reads state
    0xac, // 15: ireturn
];
const STATE_BEFORE_TRY_HANDLERS: [(usize, usize, usize); 1] = [(6, 11, 13)];

#[test]
fn a_local_the_handler_reads_is_live_before_the_protected_range() {
    let code = STATE_BEFORE_TRY;
    let (live, covered) =
        live_locals_per_pc_with_handlers(&code, code.len(), 0, &[], &STATE_BEFORE_TRY_HANDLERS);
    for pc in [2usize, 3, 4, 5] {
        assert!(covered[pc], "pc {pc} must be covered");
        assert_ne!(
            live[pc] & (1 << 1),
            0,
            "local 1 is read by the handler, the throw at pc 6 reaches it, and \
             nothing between its def at pc 1 and pc 6 redefines it — so it is \
             live at pc {pc}. live_at[{pc}] = {:#x}",
            live[pc]
        );
    }
    assert_eq!(
        live[1] & (1 << 1),
        0,
        "the exception edge must not make local 1 live before its own first def"
    );

    // The same answer from the windowed analysis the deopt snapshot uses.
    let (rows, _, words) =
        live_locals_per_pc_all(&code, code.len(), 0, &[], &STATE_BEFORE_TRY_HANDLERS, 4);
    assert_eq!(words, 1);
    assert_ne!(
        rows[4] & (1 << 1),
        0,
        "window 0 must agree with the 64-slot map"
    );

    // Control: without the exception table, local 1 is genuinely dead there.
    let (blind, _) = live_locals_per_pc_with_handlers(&code, code.len(), 0, &[], &[]);
    assert_eq!(
        blind[4] & (1 << 1),
        0,
        "precondition: with no exception edge the read is invisible, so the \
         assertion above is about the edge and not about the fixture"
    );
}

#[test]
fn a_local_the_handler_reads_interferes_with_a_temporary_that_dies_before_the_try() {
    let code = STATE_BEFORE_TRY;
    let mut blocks = build_cfg_with_handlers(&code, code.len(), &STATE_BEFORE_TRY_HANDLERS);
    for block in &mut blocks {
        compute_gen_kill(&code, block);
    }
    assert!(solve_liveness(&mut blocks, 0));
    let ranges = handler_live_in_ranges(&blocks, &STATE_BEFORE_TRY_HANDLERS);
    let interference = build_interference(&code, &blocks, 4, &ranges);
    assert_ne!(
        interference[2] & (1 << 1),
        0,
        "`tmp` (local 2) is defined at pc 3 while `state` (local 1) must \
         survive for the handler; sharing a register would hand the catch \
         block `tmp`'s value. interference[2] = {:#x}",
        interference[2]
    );
    assert_ne!(
        interference[1] & (1 << 2),
        0,
        "interference must be symmetric"
    );

    // And the allocation built on that graph keeps them apart.
    let result = allocate_registers_with_handlers(
        &code,
        code.len(),
        4,
        0,
        &[],
        &[],
        &STATE_BEFORE_TRY_HANDLERS,
    );
    if let (Some(a), Some(b)) = (result.assignments[1], result.assignments[2]) {
        assert_ne!(a, b, "locals 1 and 2 share r{a}");
    }
}
