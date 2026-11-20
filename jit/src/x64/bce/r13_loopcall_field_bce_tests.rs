use super::*;

fn alias(
    span: (usize, usize),
    synthetic: usize,
    is_array: bool,
    sites: &[(usize, usize)],
) -> FieldHoistAlias {
    FieldHoistAlias {
        loop_header: span.0,
        loop_end: span.1,
        key: sites[0].0,
        synthetic,
        is_array,
        sites: sites.to_vec(),
    }
}

fn run(
    code: &[u8],
    loops: &[(usize, usize)],
    aliases: &[FieldHoistAlias],
) -> (FxHashSet<usize>, Vec<SpeculativeBCEGuard>, Vec<usize>) {
    analyze_bounds_elimination_with_field_hoists(code, code.len(), loops, Some(&[]), aliases)
}

/// `int sum() { int s = 0; for (int i = 0; i < data.length; i++) s += data[i]; return s; }`
/// (`this` = 0, `s` = 1, `i` = 2, so `max_locals` = 3): the X2 fixture.
fn length_bound() -> Vec<u8> {
    vec![
        0x03, 0x3c, 0x03, 0x3d, // 0: s = 0; i = 0
        0x1c, // 4: iload_2   <- header
        0x2a, 0xb4, 0x00, 0x01, 0xbe, // 5: this.data.length
        0xa2, 0x00, 0x12, // 10: if_icmpge -> 28
        0x1b, 0x2a, 0xb4, 0x00, 0x01, 0x1c, 0x2e, // 13: s, this.data[i] (iaload 19)
        0x60, 0x3c, // 20: s += ..
        0x84, 0x02, 0x01, // 22: iinc 2, 1
        0xa7, 0xff, 0xeb, // 25: goto -> 4
        0x1b, 0xac, // 28: return s
    ]
}

/// `i < this.data.length` with both sites hoisted: the limit IS the
/// accessed array's length, so the check goes with no guard at all, and
/// the elision is reported as reading a field hoist.
#[test]
fn a_hoisted_length_bound_elides_statically() {
    let code = length_bound();
    let data = alias((4, 28), 3, true, &[(5, 9), (14, 18)]);
    let (safe, guards, dependent) = run(&code, &[(4, 25)], &[data]);
    assert!(
        safe.contains(&19),
        "this.data[i] under i < this.data.length"
    );
    assert!(guards.is_empty(), "a tautology needs no pre-header guard");
    assert_eq!(dependent, vec![19]);

    // Without the hoist nothing is proved (the round-12 answer).
    let (safe, guards, dependent) = run(&code, &[(4, 25)], &[]);
    assert!(!safe.contains(&19));
    assert!(guards.is_empty() && dependent.is_empty());
}

/// Every way the two reads could name different arrays keeps the check.
#[test]
fn a_length_bound_proves_nothing_unless_both_reads_are_the_slot() {
    let code = length_bound();
    // Only the body site hoisted: the exit test re-reads the field, and a
    // racing store could make it a longer array than the one indexed.
    let body_only = alias((4, 28), 3, true, &[(14, 18)]);
    let (safe, _, _) = run(&code, &[(4, 25)], &[body_only]);
    assert!(!safe.contains(&19), "the bound's read is not the slot");
    // Only the bound's site hoisted: the access reads the heap.
    let bound_only = alias((4, 28), 3, true, &[(5, 9)]);
    let (safe, _, _) = run(&code, &[(4, 25)], &[bound_only]);
    assert!(!safe.contains(&19), "the access's read is not the slot");
    // Two hoists (two receivers or two fields): two synthetic locals, and
    // one array's length says nothing about the other's.
    let two = [
        alias((4, 28), 3, true, &[(5, 9)]),
        alias((4, 28), 4, true, &[(14, 18)]),
    ];
    let (safe, guards, _) = run(&code, &[(4, 25)], &two);
    assert!(!safe.contains(&19), "another array's length");
    assert!(
        guards.is_empty(),
        "a field limit has no local to guard with"
    );
    // A hoist of some other loop is not live over this one.
    let elsewhere = alias((40, 60), 3, true, &[(5, 9), (14, 18)]);
    let (safe, _, _) = run(&code, &[(4, 25)], &[elsewhere]);
    assert!(
        !safe.contains(&19),
        "a hoist whose span does not cover the loop"
    );
    // An `int` answer for an array site is not a length.
    let as_int = alias((4, 28), 3, false, &[(5, 9), (14, 18)]);
    let (safe, _, _) = run(&code, &[(4, 25)], &[as_int]);
    assert!(!safe.contains(&19));
}

/// `int sum() { int n = size; int s = 0; for (int i = 0; i < n; i++) s += data[i]; return s; }`
/// (`this` = 0, `n` = 1, `s` = 2, `i` = 3; `max_locals` = 4).
fn local_bound() -> Vec<u8> {
    vec![
        0x2a, 0xb4, 0x00, 0x02, 0x3c, // 0: n = this.size
        0x03, 0x3d, 0x03, 0x3e, // 5: s = 0; i = 0
        0x1d, 0x1b, // 9: iload_3; iload_1   <- header
        0xa2, 0x00, 0x12, // 11: if_icmpge -> 29
        0x1c, 0x2a, 0xb4, 0x00, 0x01, 0x1d, 0x2e, // 14: s, this.data[i] (iaload 20)
        0x60, 0x3d, // 21: s += ..
        0x84, 0x03, 0x01, // 23: iinc 3, 1
        0xa7, 0xff, 0xef, // 26: goto -> 9
        0x1c, 0xac, // 29: return s
    ]
}

/// A local limit over a hoisted field array takes the ordinary guard,
/// which names the hoist (the emitter loads the array from its slot).
#[test]
fn a_local_bound_guards_the_hoisted_array() {
    let code = local_bound();
    let data = alias((9, 29), 4, true, &[(15, 19)]);
    let (safe, guards, dependent) = run(&code, &[(9, 26)], &[data]);
    assert!(safe.contains(&20));
    assert_eq!(dependent, vec![20]);
    assert_eq!(guards.len(), 1);
    let g = &guards[0];
    assert_eq!(
        (g.loop_header, g.array_local, g.bound_local, g.iv_local),
        (9, 4, 1, 3)
    );
    assert_eq!(
        g.array_field_hoist,
        Some(15),
        "keyed by the hoist's first aload"
    );
    assert_eq!(g.bound_field_hoist, None);
    assert!(g.reads_a_field_hoist());
    assert_eq!(g.covered_pcs, vec![20]);

    let (safe, guards, _) = run(&code, &[(9, 26)], &[]);
    assert!(!safe.contains(&20) && guards.is_empty());
}

/// `for (int i = 0; i < size; i++) s += data[i];` (`this` = 0, `s` = 1,
/// `i` = 2; `max_locals` = 3). With `size` an `int` field hoist the limit
/// is its slot, so the guard compares the two slots; without it the limit
/// is re-read every iteration and nothing may be elided.
#[test]
fn a_hoisted_int_field_bound_guards_against_its_slot() {
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x03, 0x3d, // 0: s = 0; i = 0
        0x1c, // 4: iload_2   <- header
        0x2a, 0xb4, 0x00, 0x02, // 5: this.size
        0xa2, 0x00, 0x12, // 9: if_icmpge -> 27
        0x1b, 0x2a, 0xb4, 0x00, 0x01, 0x1c, 0x2e, // 12: s, this.data[i] (iaload 18)
        0x60, 0x3c, // 19: s += ..
        0x84, 0x02, 0x01, // 21: iinc 2, 1
        0xa7, 0xff, 0xec, // 24: goto -> 4
        0x1b, 0xac, // 27: return s
    ];
    let data = alias((4, 27), 3, true, &[(13, 17)]);
    let size = alias((4, 27), 4, false, &[(5, 9)]);
    let (safe, guards, dependent) = run(&code, &[(4, 24)], &[data.clone(), size.clone()]);
    assert!(safe.contains(&18));
    assert_eq!(dependent, vec![18]);
    assert_eq!(guards.len(), 1);
    let g = &guards[0];
    assert_eq!((g.array_local, g.bound_local), (3, 4));
    assert_eq!(g.array_field_hoist, Some(13));
    assert_eq!(g.bound_field_hoist, Some(5));

    // The array hoisted but not the limit: the exit test re-reads
    // `this.size`, so a racing store could raise it past the guarded
    // length. No elision.
    let (safe, guards, dependent) = run(&code, &[(4, 24)], &[data]);
    assert!(!safe.contains(&18));
    assert!(guards.is_empty() && dependent.is_empty());
    // The limit hoisted but not the array: the access reads the heap,
    // which this analysis never proves.
    let (safe, guards, _) = run(&code, &[(4, 24)], &[size]);
    assert!(!safe.contains(&18));
    assert!(guards.is_empty());
}
