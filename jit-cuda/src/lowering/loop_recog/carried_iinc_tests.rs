use super::reject_carried_iinc;

// `if_icmpge +0` stands in for the loop-exit compare at pc 0; the body
// follows it and `goto` (0xA7) is the back branch.
const EXIT: [u8; 3] = [0xA2, 0x00, 0x00];

fn run(body: &[u8]) -> bool {
    let mut bytes = EXIT.to_vec();
    bytes.extend_from_slice(body);
    let back = bytes.len();
    bytes.extend_from_slice(&[0xA7, 0x00, 0x00]);
    reject_carried_iinc(&bytes, 0, back, 1).is_ok()
}

#[test]
fn rejects_iinc_of_a_local_read_from_the_previous_iteration() {
    // iinc 2, 1  with no store to local 2 in the body: `dstOff++`.
    assert!(!run(&[0x84, 2, 1]));
}

#[test]
fn accepts_iinc_of_the_induction_variable() {
    assert!(run(&[0x84, 1, 1]));
}

#[test]
fn accepts_iinc_of_a_local_stored_earlier_in_the_body() {
    // iconst_0; istore_2; iinc 2, 1
    assert!(run(&[0x03, 0x3D, 0x84, 2, 1]));
}
