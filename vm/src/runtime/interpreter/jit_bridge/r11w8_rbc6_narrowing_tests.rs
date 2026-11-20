use super::withdraw_osr_entries_inside_protected_ranges;
use cratonvm_reader::attribute::ExceptionTableEntry;

fn row(start_pc: u16, end_pc: u16, handler_pc: u16) -> ExceptionTableEntry {
    ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc,
        catch_type: 0,
    }
}

/// javac's `synchronized (x) { .. }` inside a loop: the back-edge target
/// (4) lies outside both of the block's ranges and stays enterable, while
/// entries inside the body range and the handler's own range are
/// withdrawn. This is `R11X64genSyncCost.blockLoop`'s exact table.
#[test]
fn a_synchronized_block_loop_keeps_its_header_entry() {
    let mut table = vec![-1i32; 72];
    table[4] = 0x40;
    table[20] = 0x80;
    table[62] = 0xc0;
    table[68] = 0x100;
    let rows = [row(15, 58, 61), row(61, 65, 61)];
    assert_eq!(
        withdraw_osr_entries_inside_protected_ranges(&mut table, &rows),
        2
    );
    assert_eq!(
        table[4], 0x40,
        "the loop header no range covers stays enterable"
    );
    assert_eq!(table[68], 0x100, "a pc past every range stays enterable");
    assert_eq!(table[20], -1, "an entry inside the body range is withdrawn");
    assert_eq!(
        table[62], -1,
        "an entry inside the handler's range is withdrawn"
    );
}

/// `try { for (..) { .. } } catch` in a method that also has an `athrow`:
/// the header IS covered, so its entry goes (the shape that must stay
/// refused). `end_pc` is exclusive and `start_pc` inclusive.
#[test]
fn a_loop_inside_a_try_loses_its_header_entry_and_bounds_are_half_open() {
    let mut table = vec![-1i32; 32];
    table[2] = 1;
    table[10] = 2;
    table[20] = 3;
    let rows = [row(2, 20, 25)];
    assert_eq!(
        withdraw_osr_entries_inside_protected_ranges(&mut table, &rows),
        2
    );
    assert_eq!(table[2], -1);
    assert_eq!(table[10], -1);
    assert_eq!(table[20], 3, "end_pc is exclusive");
}

/// A row reaching past the table (or an empty / inverted row) is clamped,
/// never indexed, and already-withdrawn entries are not counted twice.
#[test]
fn rows_past_the_table_and_empty_tables_are_harmless() {
    let mut table = vec![5i32, -1, 7];
    assert_eq!(
        withdraw_osr_entries_inside_protected_ranges(&mut table, &[]),
        0
    );
    assert_eq!(table, vec![5, -1, 7]);
    let rows = [row(1, 900, 0), row(9, 3, 0), row(1, 3, 0)];
    assert_eq!(
        withdraw_osr_entries_inside_protected_ranges(&mut table, &rows),
        1
    );
    assert_eq!(table, vec![5, -1, -1]);
}

/// The gate itself: the method-wide deny is kept ONLY behind
/// `CRATONVM_JIT_OSR_ATHROW=0`, and the non-empty-table case withdraws
/// entries instead. A text pin, because `compile_osr_body` needs a live
/// class manager and a backend no unit test can stand up.
#[test]
fn rbc6_denies_method_wide_only_under_the_kill_switch() {
    let src = include_str!("compile_osr_body.rs").replace("\r\n", "\n");
    let kill_switch = "has_athrow && !crate::runtime::env_cache::osr_athrow_allowed() {";
    assert!(src.contains(kill_switch), "the kill-switch deny is gone");
    let narrowed = "rbc6_withdraw_protected_entries = scan.has_athrow && !osr_exception_table";
    assert!(src.contains(narrowed), "the narrowed verdict is gone");
    assert!(
        src.contains("if rbc6_withdraw_protected_entries {"),
        "nothing consumes the narrowed verdict"
    );
    assert!(
        !src.contains("(!osr_exception_table.is_empty() || !crate::runtime::env_cache"),
        "the method-wide athrow + table deny is back"
    );
}
