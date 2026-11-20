use super::{pin_taken_stash_frame_words, refresh_taken_stash_frame_words, ObjectRef};
use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame};

fn frame() -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: "T.m:()V".to_string(),
        bci: 3,
        locals: vec![
            FrameValue::Object(0x1000),
            FrameValue::Int(7),
            FrameValue::Object(0),
            FrameValue::Object(0x2000),
        ],
        stack: vec![FrameValue::Long(-1), FrameValue::Object(0x3000)],
        monitors: vec![MonitorInfo {
            object: FrameValue::Object(0x1000),
            lock_depth: 1,
            relock: false,
        }],
        ..ReconstructedFrame::default()
    }
}

/// gch h2/fj (d10f restash): the frame's words are pinned in visiting
/// order (nulls and scalars skipped) and read back from their pins after
/// a simulated move, so a frame refused after a collection is restashed
/// at current addresses.
#[test]
fn gch_h2fj_a_refused_frame_reads_its_words_back_from_its_pins() {
    // SAFETY: never dereferenced; only addresses are compared.
    let lock = unsafe { ObjectRef::from_raw(0x8000usize as *mut u8) };
    let mut pins = vec![lock];
    let mut rf = frame();
    let n = pin_taken_stash_frame_words(&mut pins, &rf);
    assert_eq!(n, 4, "three locals/stack words and the monitor, no null");
    let seen: Vec<usize> = pins[1..].iter().map(|p| p.as_ptr() as usize).collect();
    assert_eq!(seen, vec![0x1000, 0x2000, 0x3000, 0x1000]);
    // A moving collection rewrote two of the pins in place.
    // SAFETY: as above.
    pins[1] = unsafe { ObjectRef::from_raw(0x9000usize as *mut u8) };
    pins[3] = unsafe { ObjectRef::from_raw(0xa000usize as *mut u8) };
    assert!(refresh_taken_stash_frame_words(&pins, 1, n, &mut rf));
    assert_eq!(rf.locals[0], FrameValue::Object(0x9000));
    assert_eq!(rf.locals[1], FrameValue::Int(7));
    assert_eq!(rf.locals[2], FrameValue::Object(0));
    assert_eq!(rf.locals[3], FrameValue::Object(0x2000));
    assert_eq!(rf.stack[1], FrameValue::Object(0xa000));
    assert!(matches!(rf.monitors[0].object, FrameValue::Object(0x1000)));
    assert_eq!(
        pins[0].as_ptr() as usize,
        0x8000,
        "the pins below are not read"
    );
}

/// A window that does not match the frame is refused and leaves it alone.
#[test]
fn gch_h2fj_a_mismatched_pin_window_is_refused() {
    let mut pins = Vec::new();
    let mut rf = frame();
    let n = pin_taken_stash_frame_words(&mut pins, &rf);
    assert!(
        !refresh_taken_stash_frame_words(&pins, 0, n - 1, &mut rf),
        "short count"
    );
    assert!(
        !refresh_taken_stash_frame_words(&pins, 1, n, &mut rf),
        "past the end"
    );
    pins.truncate(2);
    assert!(
        !refresh_taken_stash_frame_words(&pins, 0, n, &mut rf),
        "truncated"
    );
    assert_eq!(rf.locals[0], FrameValue::Object(0x1000));
    let mut empty = ReconstructedFrame::default();
    assert_eq!(pin_taken_stash_frame_words(&mut pins, &empty), 0);
    assert!(refresh_taken_stash_frame_words(
        &pins,
        pins.len(),
        0,
        &mut empty
    ));
}
