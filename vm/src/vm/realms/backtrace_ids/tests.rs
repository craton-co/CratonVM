use super::*;
use crate::native::registry::StackTraceEntry;

fn compiled(label: &Arc<str>, owner: u32, bci: i32) -> BacktraceFrame {
    BacktraceFrame::Compiled {
        label: Arc::clone(label),
        owner_class_id: owner,
        bci,
        cp_stamp: None,
    }
}

fn inlined(label: &Arc<str>, class_id: u32, bci: u32, stamp: Option<u64>) -> BacktraceFrame {
    BacktraceFrame::Inlined {
        label: Arc::clone(label),
        class_id,
        bci,
        cp_stamp: stamp,
        hidden: None,
    }
}

fn shape(frames: &[BacktraceFrame]) -> Vec<String> {
    frames.iter().map(|f| format!("{f:?}")).collect()
}

#[test]
fn a_holder_round_trips_every_frame_and_its_bci() {
    let ids = BacktraceFrameIds::new();
    let a: Arc<str> = Arc::from("p/A.run()V");
    let b: Arc<str> = Arc::from("p/B.call(I)I");
    let frames = vec![
        compiled(&a, 7, -1),
        compiled(&a, 7, 12),
        inlined(&b, 9, 3, Some(4)),
        compiled(&b, 9, 0),
    ];
    let words = ids.encode(&frames, 5).expect("every frame has an id");
    assert_eq!(words.len(), HOLDER_HEADER_WORDS + frames.len());
    assert_eq!(words[0], HOLDER_MAGIC);
    assert_eq!(words[1], 5);
    assert!(is_holder_words(words[0], words.len()));
    // `a` twice at two bcis is one id; three templates in all.
    assert_eq!(ids.len(), 3);
    let back = ids
        .decode(&words[HOLDER_HEADER_WORDS..])
        .expect("known ids");
    assert_eq!(shape(&back), shape(&frames));
    // A second capture through the same frames hands out nothing new.
    let again = ids.encode(&frames[1..], 6).expect("repeat");
    assert_eq!(ids.len(), 3);
    assert_eq!(
        &again[HOLDER_HEADER_WORDS..],
        &words[HOLDER_HEADER_WORDS + 1..]
    );
}

#[test]
fn a_different_stamp_or_class_is_a_different_frame() {
    let ids = BacktraceFrameIds::new();
    let a: Arc<str> = Arc::from("p/A.run()V");
    let frames = vec![
        inlined(&a, 1, 2, None),
        inlined(&a, 1, 2, Some(1)),
        inlined(&a, 2, 2, None),
    ];
    let words = ids.encode(&frames, 0).expect("ids");
    assert_eq!(ids.len(), 3);
    let back = ids
        .decode(&words[HOLDER_HEADER_WORDS..])
        .expect("known ids");
    assert_eq!(shape(&back), shape(&frames));
}

#[test]
fn an_entry_frame_keeps_the_registry() {
    let ids = BacktraceFrameIds::new();
    let a: Arc<str> = Arc::from("p/A.run()V");
    let entry = BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("p/A"),
        method_name: Arc::from("run"),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: -1,
        class_id: None,
        method_index: None,
    });
    assert!(ids.encode(&[compiled(&a, 1, 0), entry], 0).is_none());
    assert!(ids.encode(&[], 0).is_some(), "an empty trace is a holder");
}

#[test]
fn an_unknown_id_does_not_decode() {
    let ids = BacktraceFrameIds::new();
    assert!(ids.decode(&[pack(0, 0)]).is_none());
    assert_eq!(ids.decode(&[]).map(|f| f.len()), Some(0));
    assert!(!is_holder_words(0, 4));
    assert!(!is_holder_words(HOLDER_MAGIC, 1));
}
