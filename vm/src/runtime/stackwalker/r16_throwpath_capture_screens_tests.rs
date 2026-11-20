use super::{
    backtrace_frame_hidden_decided, backtrace_frame_hidden_decided_scanned,
    backtrace_frame_identity, backtrace_frame_identity_scanned, capture_screens,
    is_thread_run_with_frame, label_may_be_hidden, label_may_be_hidden_scanned,
    lambda_body_named, lambda_body_named_screened, may_need_loader_middle_frames,
    may_need_thread_run_middle_frame, split_label_identity, BacktraceFrame, CaptureScreens,
    LINE_NUMBER_UNKNOWN,
};
use crate::classloading::ClassId;
use crate::native::registry::StackTraceEntry;
use std::sync::Arc;

const LABELS: &[&str] = &[
    "probe/Task.run:()V",
    "probe/Task.run:(I)V",
    "java/lang/Thread.run:()V",
    "java/lang/Thread.runWith:(Ljava/lang/Object;Ljava/lang/Runnable;)V",
    "probe/Main.lambda$main$0:()V",
    "probe/Main.f$anonfun$1:()V",
    "probe/Main.dollar$:()V",
    "java/lang/ClassLoader.loadClass:(Ljava/lang/String;)Ljava/lang/Class;",
    "java/lang/ClassLoader.loadClass:(Ljava/lang/String;Z)Ljava/lang/Class;",
    "probe/Loader.findClass:(Ljava/lang/String;)Ljava/lang/Class;",
    "probe/Holder.<clinit>:()V",
    "probe/Lambda/0x0000a.run:()V",
    "probe/Ox0.m:()V",
    "jdk/internal/X.m:()V",
    "sun/misc/Y.m:()V",
    "R16ThrowMemo.recurse:(I)Ljava/lang/Throwable;",
    "a.b.c:(La.b;)V",
    "a:b",
    "no-colon-at-all",
    "C.m:",
    ".:",
    "",
    "p/C.m:()V:tail",
    "p/\u{e9}t\u{e9}.\u{fc}:()V",
];

fn compiled(label: &str, owner: u32) -> BacktraceFrame {
    BacktraceFrame::Compiled {
        label: Arc::from(label),
        owner_class_id: owner,
        bci: 3,
        cp_stamp: None,
    }
}

fn inlined(label: &str, class_id: u32) -> BacktraceFrame {
    BacktraceFrame::Inlined {
        label: Arc::from(label),
        class_id,
        bci: 3,
        cp_stamp: None,
        hidden: None,
    }
}

fn entry(class: &str, method: &str, descriptor: Option<&str>) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: descriptor.map(Arc::from),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: 0,
        class_id: Some(ClassId::new(4)),
        method_index: None,
    })
}

fn pool() -> Vec<BacktraceFrame> {
    let mut out = Vec::new();
    for label in LABELS {
        out.push(compiled(label, 5));
        out.push(inlined(label, 0));
        out.push(inlined(label, 6));
    }
    out.push(entry("probe/Task", "run", None));
    out.push(entry("java/lang/Thread", "runWith", Some("()V")));
    out.push(entry("java/lang/ClassLoader", "loadClass", None));
    out.push(entry("probe/0xabc", "m", Some("()V")));
    out.push(entry("probe/Main", "main", Some("([Ljava/lang/String;)V")));
    out
}

#[test]
fn the_label_split_is_the_two_rsplits() {
    for label in LABELS {
        let reference = label
            .rsplit_once(':')
            .and_then(|(owner_and_method, descriptor)| {
                owner_and_method
                    .rsplit_once('.')
                    .map(|(class, method)| (class, method, descriptor))
            });
        assert_eq!(split_label_identity(label), reference, "{label:?}");
    }
}

#[test]
fn every_frame_is_named_and_judged_alike() {
    for frame in pool() {
        assert_eq!(
            backtrace_frame_identity_scanned(&frame),
            backtrace_frame_identity(&frame),
            "{frame:?}"
        );
        assert_eq!(
            backtrace_frame_hidden_decided_scanned(&frame),
            backtrace_frame_hidden_decided(&frame),
            "{frame:?}"
        );
    }
    for label in LABELS {
        assert_eq!(
            label_may_be_hidden_scanned(label),
            label_may_be_hidden(label),
            "{label:?}"
        );
        let method = split_label_identity(label).map_or(*label, |(_, m, _)| m);
        for name in [*label, method] {
            assert_eq!(
                lambda_body_named_screened(name),
                lambda_body_named(name),
                "{name:?}"
            );
        }
    }
}

fn reference(trace: &[BacktraceFrame]) -> CaptureScreens {
    CaptureScreens {
        thread_run_middle: trace
            .windows(2)
            .any(|pair| may_need_thread_run_middle_frame(&pair[0], &pair[1])),
        loader_middle: trace
            .windows(2)
            .any(|pair| may_need_loader_middle_frames(&pair[0], &pair[1])),
        hidden_quiet: trace
            .iter()
            .all(|f| backtrace_frame_hidden_decided(f) == Some(false)),
        run_with: trace.iter().any(is_thread_run_with_frame),
    }
}

/// Every ordered pair and triple of the pool, and the empty and one-frame
/// traces.
#[test]
fn one_pass_answers_what_the_three_screens_answer() {
    let pool = pool();
    assert_eq!(capture_screens(&[]), reference(&[]));
    for a in &pool {
        let one = [a.clone()];
        assert_eq!(capture_screens(&one), reference(&one), "{one:?}");
        for b in &pool {
            let two = [a.clone(), b.clone()];
            assert_eq!(capture_screens(&two), reference(&two), "{two:?}");
        }
    }
    // Triples over a smaller slice of the pool (the pairs above already
    // cover every caller/callee combination; a triple checks that each
    // field keeps the first `true` and the pairing moves on).
    let small: Vec<_> = pool.iter().step_by(5).cloned().collect();
    for a in &small {
        for b in &small {
            for c in &small {
                let three = [a.clone(), b.clone(), c.clone()];
                assert_eq!(capture_screens(&three), reference(&three), "{three:?}");
            }
        }
    }
}
