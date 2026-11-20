use super::indy_rows_have_trap;

/// SY4-2: only a row the 0xba arm would lower to the reason-8 trap
/// refuses the self-locking run up front; a fully bridged method keeps it.
#[test]
fn only_an_unbridged_row_is_a_trap() {
    const BRIDGE: usize = 0x7000;
    let concat = (3usize, 1usize, b'L', vec![b'I'], 0x9000usize);
    assert!(!indy_rows_have_trap(&[], BRIDGE), "no indy at all");
    assert!(
        !indy_rows_have_trap(&[concat.clone()], BRIDGE),
        "a bridged concat"
    );
    let unknown_bootstrap = (9usize, 0usize, b'L', Vec::new(), 0usize);
    assert!(indy_rows_have_trap(
        &[concat.clone(), unknown_bootstrap],
        BRIDGE
    ));
    let void_site = (9usize, 0usize, b'V', Vec::new(), 0x9100usize);
    assert!(
        indy_rows_have_trap(&[void_site], BRIDGE),
        "a void indy is never bridged"
    );
    assert!(
        indy_rows_have_trap(&[concat], 0),
        "the bridge helper unwired"
    );
}
