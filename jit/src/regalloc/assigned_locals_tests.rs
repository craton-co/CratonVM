use super::*;

/// The row of `pc` in a one-word table.
fn row(table: &(Vec<u64>, Vec<bool>, usize), pc: usize) -> u64 {
    assert_eq!(table.2, 1, "one word per pc");
    assert!(table.1[pc], "pc {pc} is covered");
    table.0[pc]
}

/// `int t = 42; int i = 0; while (i < 1000) i++; return i;` with `t`
/// never read: dead everywhere, but assigned from pc 3 on, the loop
/// header included, where liveness calls it dead.
#[test]
fn a_dead_local_stays_assigned_across_a_loop() {
    #[rustfmt::skip]
    let code: [u8; 22] = [
        0x10, 42,         // 0:  bipush 42
        0x3c,             // 2:  istore_1        t
        0x03,             // 3:  iconst_0
        0x3b,             // 4:  istore_0        i
        0x1a,             // 5:  iload_0         <- loop header
        0x11, 0x03, 0xe8, // 6:  sipush 1000
        0xa2, 0x00, 0x09, // 9:  if_icmpge +9 -> 18
        0x84, 0x00, 0x01, // 12: iinc 0, 1
        0xa7, 0xff, 0xf6, // 15: goto -10 -> 5
        0x1a,             // 18: iload_0
        0xac,             // 19: ireturn
        0, 0,
    ];
    let assigned = assigned_locals_per_pc_all(&code, 20, 0, &[], &[], 2);
    assert_eq!(row(&assigned, 0), 0, "nothing on entry");
    assert_eq!(row(&assigned, 3), 0b10, "t after its store");
    assert_eq!(row(&assigned, 5), 0b11, "both at the header");
    assert_eq!(row(&assigned, 18), 0b11);
    let (live, _, _) = live_locals_per_pc_all(&code, 20, 0, &[], &[], 2);
    assert_eq!(live[5] & 0b10, 0, "t is dead at the header");
}

/// A slot one arm of a diamond does not store is not assigned at the
/// merge; a parameter is assigned everywhere.
#[test]
fn a_slot_one_path_skips_is_not_assigned_at_the_merge() {
    #[rustfmt::skip]
    let code: [u8; 11] = [
        0x1a,             // 0: iload_0          (parameter)
        0x99, 0x00, 0x06, // 1: ifeq +6 -> 7
        0x04,             // 4: iconst_1
        0x3c,             // 5: istore_1
        0x00,             // 6: nop
        0x1a,             // 7: iload_0          <- merge
        0xac,             // 8: ireturn
        0, 0,
    ];
    let assigned = assigned_locals_per_pc_all(&code, 9, 1, &[], &[], 2);
    assert_eq!(row(&assigned, 0), 0b01, "the parameter on entry");
    assert_eq!(row(&assigned, 6), 0b11, "the storing arm");
    assert_eq!(row(&assigned, 7), 0b01, "one path skips the store");
}

/// A handler is entered with its protected block's ENTRY state: the
/// throw may come before the block's store.
#[test]
fn a_handler_sees_the_protected_blocks_entry_state() {
    #[rustfmt::skip]
    let code: [u8; 9] = [
        0x04, // 0: iconst_1          <- protected [0, 4)
        0x3c, // 1: istore_1
        0x03, // 2: iconst_0
        0xac, // 3: ireturn
        0x4d, // 4: astore_2          <- handler
        0x1a, // 5: iload_0
        0xac, // 6: ireturn
        0, 0,
    ];
    let assigned = assigned_locals_per_pc_all(&code, 7, 1, &[], &[(0, 4, 4)], 3);
    assert_eq!(row(&assigned, 2), 0b011);
    assert_eq!(
        row(&assigned, 4),
        0b001,
        "slot 1 may be unassigned at the throw"
    );
    assert_eq!(row(&assigned, 5), 0b101, "the caught exception");
}

/// `int t = 42; int i = 0; while (i < 1000) i++; return i;` — the loop
/// of [`a_dead_local_stays_assigned_across_a_loop`], `t` never read.
#[rustfmt::skip]
const DEAD_T_LOOP: [u8; 22] = [
    0x10, 42,         // 0:  bipush 42
    0x3c,             // 2:  istore_1        t
    0x03,             // 3:  iconst_0
    0x3b,             // 4:  istore_0        i
    0x1a,             // 5:  iload_0         <- loop header
    0x11, 0x03, 0xe8, // 6:  sipush 1000
    0xa2, 0x00, 0x09, // 9:  if_icmpge +9 -> 18
    0x84, 0x00, 0x01, // 12: iinc 0, 1
    0xa7, 0xff, 0xf6, // 15: goto -10 -> 5
    0x1a,             // 18: iload_0
    0xac,             // 19: ireturn
    0, 0,
];

/// Interpreter round i1 wave 21, lane L3: a store to `u` while `t` holds
/// a value it will never be asked for again still may not land in `t`'s
/// home. Ordinary liveness gives the two no edge (`t` is dead before `u`
/// is stored); the keep-alive table does, at `u`'s store and at every pc
/// both are assigned.
#[test]
fn a_store_interferes_with_every_local_assigned_across_it() {
    #[rustfmt::skip]
    let code: [u8; 11] = [
        0x10, 42, // 0: bipush 42
        0x3b,     // 2: istore_0         t
        0x1a,     // 3: iload_0          t's last read
        0x57,     // 4: pop
        0x03,     // 5: iconst_0
        0x3c,     // 6: istore_1         u
        0x1b,     // 7: iload_1
        0xac,     // 8: ireturn
        0, 0,
    ];
    let (assigned, _, words) = assigned_locals_per_pc_all(&code, 9, 0, &[], &[], 2);
    let mut interference = vec![0u64; 2];
    keep_assigned_interference(&code, 9, 2, &assigned, words, &mut interference);
    assert_eq!(interference, vec![0b10, 0b01], "t and u interfere");
    // An empty table widens nothing.
    let mut untouched = vec![0u64; 2];
    keep_assigned_interference(&code, 9, 2, &[], 0, &mut untouched);
    assert_eq!(untouched, vec![0, 0]);
}

/// The allocation under the keep-alive: `t` and the loop counter never
/// share a home, and the loop header's block live-in names `t`, so the
/// OSR trampoline seeds it (and masks a dead local coalesced with it)
/// instead of treating it as dead. Without the table the allocation is
/// the ordinary one.
#[test]
fn the_keep_alive_allocation_keeps_a_dead_assigned_local_home() {
    let (assigned, _, words) = assigned_locals_per_pc_all(&DEAD_T_LOOP, 20, 0, &[], &[], 2);
    let kept = allocate_registers_keeping_assigned(
        &DEAD_T_LOOP,
        20,
        2,
        0,
        &[],
        &[],
        &[],
        &assigned,
        words,
    );
    if let (Some(i), Some(t)) = (kept.assignments[0], kept.assignments[1]) {
        assert_ne!(i, t, "i and t share r{i}");
    }
    let header_live_in = |result: &RegAllocResult| {
        result
            .block_live_in
            .iter()
            .find(|(pc, _)| *pc == 5)
            .map(|(_, live_in)| *live_in)
    };
    assert_eq!(
        header_live_in(&kept).map(|m| m & 0b10),
        Some(0b10),
        "t is seeded at the header"
    );
    let plain = allocate_registers_with_handlers(&DEAD_T_LOOP, 20, 2, 0, &[], &[], &[]);
    assert_eq!(
        header_live_in(&plain).map(|m| m & 0b10),
        Some(0),
        "without a debugger t is dead there"
    );
    // A table that keeps nothing allocates exactly as without one.
    let zeros = vec![0u64; assigned.len()];
    let none_kept = allocate_registers_keeping_assigned(
        &DEAD_T_LOOP,
        20,
        2,
        0,
        &[],
        &[],
        &[],
        &zeros,
        words,
    );
    assert_eq!(none_kept.assignments, plain.assignments);
    assert_eq!(none_kept.xmm_assignments, plain.xmm_assignments);
    assert_eq!(none_kept.block_live_in, plain.block_live_in);
}
