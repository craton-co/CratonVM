use super::body_shape;

#[test]
fn body_shape_counts_invokes_and_steps_whole_instructions() {
    // aload_0; getfield #2; invokevirtual #3; ireturn
    let code = [0x2a, 0xb4, 0x00, 0x02, 0xb6, 0x00, 0x03, 0xac];
    assert_eq!(body_shape(&code), (8, 1));
    // The getfield operand byte 0xb6 must not count as an invoke.
    let code = [0x2a, 0xb4, 0xb6, 0xb6, 0xac];
    assert_eq!(body_shape(&code), (5, 0));
}
