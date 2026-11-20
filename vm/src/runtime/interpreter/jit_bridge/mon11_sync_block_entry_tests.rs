use super::{
    is_javac_monitor_release_row, withdraw_osr_entries_inside_protected_ranges,
    withdraw_osr_entries_inside_protected_ranges_except,
};
use cratonvm_reader::attribute::ExceptionTableEntry;

fn row(start_pc: u16, end_pc: u16, handler_pc: u16, catch_type: u16) -> ExceptionTableEntry {
    ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc,
        catch_type,
    }
}

/// javac 25 for
/// `static void m(Object o, int n) { synchronized (o) { for (int i = 0; i < n; i++) count++; } }`:
/// the header (6) lies inside the body row [4, 27).
fn javac_sync_loop() -> (Vec<u8>, [ExceptionTableEntry; 2]) {
    let code = vec![
        0x2a, // 0: aload_0
        0x59, // 1: dup
        0x4d, // 2: astore_2
        0xc2, // 3: monitorenter
        0x03, // 4: iconst_0
        0x3e, // 5: istore_3
        0x1d, // 6: iload_3   <- loop header
        0x1b, // 7: iload_1
        0xa2, 0x00, 0x11, // 8: if_icmpge 25
        0xb2, 0x00, 0x07, // 11: getstatic
        0x04, // 14: iconst_1
        0x60, // 15: iadd
        0xb3, 0x00, 0x07, // 16: putstatic
        0x84, 0x03, 0x01, // 19: iinc 3 1
        0xa7, 0xff, 0xf0, // 22: goto 6
        0x2c, // 25: aload_2
        0xc3, // 26: monitorexit
        0xa7, 0x00, 0x0a, // 27: goto 37
        0x3a, 0x04, // 30: astore 4
        0x2c, // 32: aload_2
        0xc3, // 33: monitorexit
        0x19, 0x04, // 34: aload 4
        0xbf, // 36: athrow
        0xb1, // 37: return
    ];
    (code, [row(4, 27, 30, 0), row(30, 34, 30, 0)])
}

#[test]
fn both_javac_synchronized_rows_are_recognised() {
    let (code, rows) = javac_sync_loop();
    assert!(is_javac_monitor_release_row(&code, &rows[0]), "body row");
    assert!(
        is_javac_monitor_release_row(&code, &rows[1]),
        "handler's own row"
    );
}

#[test]
fn other_shapes_are_not_monitor_release_rows() {
    let (code, rows) = javac_sync_loop();
    // A typed catch is a real `try`.
    assert!(!is_javac_monitor_release_row(&code, &row(4, 27, 30, 9)));
    // The body row must start right after `astore l; monitorenter` of the
    // local the handler releases.
    let mut other_lock = code.clone();
    other_lock[2] = 0x4c; // astore_1
    assert!(!is_javac_monitor_release_row(&other_lock, &rows[0]));
    assert!(!is_javac_monitor_release_row(&code, &row(5, 27, 30, 0)));
    // A handler that does not rethrow what it stored.
    let mut swallowed = code.clone();
    swallowed[36] = 0xb1; // return
    assert!(!is_javac_monitor_release_row(&swallowed, &rows[0]));
    let mut other_thrown = code.clone();
    other_thrown[35] = 0x05;
    assert!(!is_javac_monitor_release_row(&other_thrown, &rows[0]));
    // Past the end of the code: no panic.
    assert!(!is_javac_monitor_release_row(&code, &row(4, 27, 900, 0)));
    assert!(!is_javac_monitor_release_row(&code, &row(0, 27, 30, 0)));
}

#[test]
fn the_header_inside_the_block_is_kept_only_when_its_rows_are_skipped() {
    let (code, rows) = javac_sync_loop();
    let flags: Vec<bool> = rows
        .iter()
        .map(|r| is_javac_monitor_release_row(&code, r))
        .collect();
    let mut table = vec![-1i32; code.len()];
    table[6] = 0x40;
    let mut plain = table.clone();
    assert_eq!(
        withdraw_osr_entries_inside_protected_ranges(&mut plain, &rows),
        1
    );
    assert_eq!(plain[6], -1, "switch off: RBC.6 withdraws the header");
    assert_eq!(
        withdraw_osr_entries_inside_protected_ranges_except(&mut table, &rows, |i| flags[i]),
        0
    );
    assert_eq!(table[6], 0x40, "switch on: the header stays enterable");
    // A `try` around the block still withdraws it.
    let mut wrapped = vec![-1i32; code.len()];
    wrapped[6] = 0x40;
    let with_try = [rows[0].clone(), rows[1].clone(), row(0, 37, 30, 9)];
    assert_eq!(
        withdraw_osr_entries_inside_protected_ranges_except(&mut wrapped, &with_try, |i| {
            flags.get(i).copied().unwrap_or(false)
        }),
        1
    );
    assert_eq!(wrapped[6], -1);
}
