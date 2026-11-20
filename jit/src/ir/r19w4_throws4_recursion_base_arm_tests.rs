use super::*;

/// `static void down(int d) { if (d <= 1) throw E; down(d - 1); }`.
const DOWN: [u8; 16] = [
    0x1a, // 0: iload_0
    0x04, // 1: iconst_1
    0xa3, 0x00, 0x07, // 2: if_icmpgt -> 9
    0xb2, 0x00, 0x02, // 5: getstatic #2
    0xbf, // 8: athrow
    0x1a, // 9: iload_0
    0x04, // 10: iconst_1
    0x64, // 11: isub
    0xb8, 0x00, 0x03, // 12: invokestatic #3 (self)
    0xb1, // 15: return
];

fn named(code: &[u8], param_slots: usize, self_cp: Option<u16>) -> bool {
    let is_self = |cp: u16| Some(cp) == self_cp;
    cold_throw_arm_tests_recursion_parameter(code, code.len(), param_slots, &is_self)
}

#[test]
fn the_base_case_throw_arm_of_a_self_recursion_is_named() {
    assert!(named(&DOWN, 1, Some(3)));
}

#[test]
fn a_body_that_does_not_call_itself_keeps_its_cold_arm() {
    assert!(!named(&DOWN, 1, None));
}

#[test]
fn an_arm_on_a_local_that_is_not_a_parameter_keeps_its_cold_arm() {
    assert!(!named(&DOWN, 0, Some(3)));
}

#[test]
fn an_arm_on_an_unstepped_parameter_keeps_its_cold_arm() {
    // `if (d <= 1) throw E; down(d);` -- the recursion does not step `d`.
    let mut code = DOWN;
    code[10] = 0x00; // nop
    code[11] = 0x00; // nop
    assert!(!named(&code, 1, Some(3)));
}
