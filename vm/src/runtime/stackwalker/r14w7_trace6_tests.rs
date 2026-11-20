use super::r13_trace3_standin_tests::{add_class, java, pool};
use super::*;

const INTERRUPTED: &str = "java/lang/InterruptedException";

fn at(class_id: ClassId, bci: i32) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("probe/Main"),
        method_name: Arc::from("main"),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(class_id),
        method_index: None,
    })
}

/// `new #2; dup; invokespecial #6; athrow` per `IllegalArgumentException`
/// site, padded with `nop` so each `invokespecial` lands at its pc.
fn iae_body(sites: &[usize]) -> Vec<u8> {
    let mut code = Vec::new();
    for &site in sites {
        while code.len() + 4 < site {
            code.push(0x00);
        }
        code.extend_from_slice(&[0xbb, 0, 2, 0x59, 0xb7, 0, 6, 0xbf]);
    }
    code.push(0xb1);
    code
}

/// JDK-25-shaped `Thread.join(long, int)` (its two checks at pcs 4 and 12,
/// lines as in the JDK 25 sources), `sleep(long, int)` with its checks at
/// `sleep_sites`, a `join(long)` with one site, and an
/// `Object.wait(long, int)` (no two-site row). `probe/Main.main()V` calls
/// `join(JI)` at bci 1, `sleep(JI)` at 4, `join()` at 7, `join(J)` at 10
/// and `wait(JI)` at 13.
fn fixture(sleep_sites: &[usize]) -> (ClassStore, ClassId) {
    let iae = (
        STANDIN_ARG_CHECK_THROWABLE,
        "<init>",
        "(Ljava/lang/String;)V",
    );
    let mut store = ClassStore::new();
    let object = add_class(
        &mut store,
        "java/lang/Object",
        None,
        pool(&[iae]),
        vec![java("wait", "(JI)V", iae_body(&[4]), &[(0, 492)])],
    );
    add_class(
        &mut store,
        "java/lang/Thread",
        Some(object),
        pool(&[iae]),
        vec![
            java("join", "(JI)V", iae_body(&[4, 12]), &[(0, 1924), (8, 1928)]),
            java(
                "sleep",
                "(JI)V",
                iae_body(sleep_sites),
                &[(0, 567), (8, 571)],
            ),
            java("join", "(J)V", iae_body(&[4]), &[(0, 1865)]),
            java("join", "()V", vec![0xb1], &[(0, 1963)]),
        ],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            ("java/lang/Thread", "join", "(JI)V"),
            ("java/lang/Thread", "sleep", "(JI)V"),
            ("java/lang/Thread", "join", "()V"),
            ("java/lang/Thread", "join", "(J)V"),
            ("java/lang/Object", "wait", "(JI)V"),
        ]),
        vec![java(
            "main",
            "()V",
            vec![
                0x2a, 0xb6, 0, 6, 0xb8, 0, 12, 0xb6, 0, 18, 0xb6, 0, 24, 0xb6, 0, 30, 0xb1,
            ],
            &[(0, 3)],
        )],
    );
    (store, main)
}

fn one_frame(frames: Option<Vec<BacktraceFrame>>) -> Option<(String, i32, i32)> {
    let frames = frames?;
    assert_eq!(frames.len(), 1);
    let e = frames[0].to_entry();
    Some((
        format!("{}.{}", e.class_name, e.method_name),
        e.line_number,
        e.byte_code_index,
    ))
}

/// TW6-3: the native's bit picks the check -- `millis < 0` the first
/// site, the `nanos` range the second -- for `join(long, int)` and
/// `sleep(long, int)`; anything else answers nothing.
#[test]
fn a_two_site_argument_check_takes_the_site_the_native_names() {
    let (store, main) = fixture(&[4, 12]);
    let store = &store;
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let mut got = |bci: i32, throwable: &str, second: bool| {
        one_frame(served_arg_check_standin_frames(
            store,
            &at(main, bci),
            throwable,
            second,
            &mut resolve,
        ))
    };
    let frame = |m: &str, line: i32, bci: i32| Some((m.to_string(), line, bci));
    let iae = STANDIN_ARG_CHECK_THROWABLE;
    assert_eq!(got(1, iae, false), frame("java/lang/Thread.join", 1924, 4));
    assert_eq!(got(1, iae, true), frame("java/lang/Thread.join", 1928, 12));
    assert_eq!(got(4, iae, false), frame("java/lang/Thread.sleep", 567, 4));
    assert_eq!(got(4, iae, true), frame("java/lang/Thread.sleep", 571, 12));
    // Not an `IllegalArgumentException`.
    assert_eq!(got(1, INTERRUPTED, true), None);
    // Not a two-site row: `join()`, `join(long)` (the one-site rule's),
    // `Object.wait(long, int)`.
    assert_eq!(got(7, iae, true), None);
    assert_eq!(got(10, iae, false), None);
    assert_eq!(got(13, iae, false), None);
    // Not at an invoke.
    assert_eq!(got(0, iae, false), None);
}

/// A row whose body does not have exactly two sites is refused, not
/// guessed.
#[test]
fn a_two_site_row_with_another_site_count_gets_nothing() {
    for sites in [&[4usize][..], &[4, 12, 20][..]] {
        let (store, main) = fixture(sites);
        let store = &store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        for second in [false, true] {
            let got = served_arg_check_standin_frames(
                store,
                &at(main, 4),
                STANDIN_ARG_CHECK_THROWABLE,
                second,
                &mut resolve,
            );
            assert!(got.is_none(), "{sites:?} {second}");
        }
    }
}

/// TW6-2: only an `InterruptedException` at a `join()` call, and only
/// when the native said so.
#[test]
fn a_served_join_declines_only_when_told_at_a_join_call() {
    let (store, main) = fixture(&[4, 12]);
    let declined = |bci: i32, throwable: &str, declines: bool| {
        served_join_declines_standin_frames(&store, &at(main, bci), throwable, declines)
    };
    assert!(declined(7, INTERRUPTED, true));
    assert!(!declined(7, INTERRUPTED, false));
    assert!(!declined(7, STANDIN_ARG_CHECK_THROWABLE, true));
    assert!(
        !declined(1, INTERRUPTED, true),
        "join(long, int) declines by no hint"
    );
    assert!(
        !declined(10, INTERRUPTED, true),
        "join(long) declines by no hint"
    );
}

/// RV6-2: the opcode screen, and no answer for a frame whose code is not
/// at hand.
#[test]
fn the_receiver_npe_screen_reads_only_instance_invokes() {
    // invokevirtual, invokespecial, invokeinterface, invokestatic, athrow.
    let code = [0xb6, 0, 1, 0xb7, 0, 1, 0xb9, 0, 1, 1, 0, 0xb8, 0, 1, 0xbf];
    assert!(code_is_instance_invoke_at(&code, 0));
    assert!(code_is_instance_invoke_at(&code, 3));
    assert!(code_is_instance_invoke_at(&code, 6));
    assert!(!code_is_instance_invoke_at(&code, 11));
    assert!(!code_is_instance_invoke_at(&code, 14));
    assert!(!code_is_instance_invoke_at(&code, 40));
    let (_, main) = fixture(&[4, 12]);
    assert!(!receiver_npe_gets_no_standin_frames(
        &at(main, 1),
        "java/lang/NullPointerException"
    ));
}
