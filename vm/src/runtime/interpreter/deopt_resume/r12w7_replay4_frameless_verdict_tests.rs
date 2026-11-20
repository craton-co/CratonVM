use super::{door_rerun_verdict, DoorRerunCause};

/// `putstatic` at bci 1, then a storing loop (the wave-4 fixture).
const STORE_THEN_LOOP: [u8; 23] = [
    0x04, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0x05, 0xb3, 0x00,
    0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
];

#[test]
fn a_body_whose_frameless_traps_replay_exactly_reruns_exactly() {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut ran = cratonvm_jit::CompiledMethod::new(buf);
    let code = STORE_THEN_LOOP;
    assert_eq!(
        door_rerun_verdict(&code, &ran, "C", "m", "()V", None),
        (DoorRerunCause::Frameless, false),
        "unstamped: the whole-body rule"
    );
    ran.frameless_traps_replay_exact = true;
    assert_eq!(
        door_rerun_verdict(&code, &ran, "C", "m", "()V", None),
        (DoorRerunCause::Frameless, true)
    );
}
