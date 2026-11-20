use super::*;

/// Only a pc with a `field_info` row whose index the declaring-class
/// answer repeats gets a row; one answer per constant-pool index.
#[test]
fn rows_need_a_field_row_and_the_same_index() {
    // pc 1: cp 5 (index 2, class 100); pc 4: cp 5 again; pc 7: cp 6,
    // whose declaring-class answer names another index; pc 10: cp 7, no
    // `field_info` row; pc 13: cp 8, no answer; pc 16: cp 9, class 0.
    let field_ops = [(1usize, 5u16), (4, 5), (7, 6), (10, 7), (13, 8), (16, 9)];
    let fm: std::collections::HashMap<usize, (usize, u8)> = std::collections::HashMap::from([
        (1, (2, b'I')),
        (4, (2, b'I')),
        (7, (3, b'I')),
        (13, (4, b'I')),
        (16, (5, b'I')),
    ]);
    let asks = std::cell::Cell::new(0usize);
    let decl_of = |cp: u16| -> Option<(usize, u32)> {
        asks.set(asks.get() + 1);
        match cp {
            5 => Some((2, 100)),
            6 => Some((9, 200)),
            7 => Some((0, 300)),
            9 => Some((5, 0)),
            _ => None,
        }
    };
    let rows = ir_field_decl_rows(&field_ops, &fm, &decl_of);
    assert_eq!(rows, std::collections::HashMap::from([(1, 100), (4, 100)]));
    // cp 5 once, 6, 8 and 9 once each; cp 7 has no field row to ask for.
    assert_eq!(asks.get(), 4);
}
