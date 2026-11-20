use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;

struct Fixture {
    store: ClassStore,
    thread: ClassId,
    worker: ClassId,
    own_run: ClassId,
    task: ClassId,
    sub_task: ClassId,
    other: ClassId,
}

/// JDK 25's `Thread.run()`: `scopedValueBindings()` at pc 0 (line 1472),
/// `runWith(bindings, task)` at pc 7 (line 1474).
fn jdk25_run() -> ClassFileMethod {
    java(
        "run",
        "()V",
        vec![0xb8, 0, 12, 0x4c, 0x2a, 0x2b, 0x2b, 0xb7, 0, 6, 0xb1],
        &[(0, 1472), (4, 1474)],
    )
}

fn fixture(run: ClassFileMethod) -> Fixture {
    let mut store = ClassStore::new();
    let thread = add_class(
        &mut store,
        "java/lang/Thread",
        None,
        pool(&[
            (
                "java/lang/Thread",
                "runWith",
                "(Ljava/lang/Object;Ljava/lang/Runnable;)V",
            ),
            (
                "java/lang/Thread",
                "scopedValueBindings",
                "()Ljava/lang/Object;",
            ),
        ]),
        vec![run],
    );
    let body = || java("run", "()V", vec![0xb1], &[(0, 10)]);
    let worker = add_class(&mut store, "probe/Worker", Some(thread), pool(&[]), vec![]);
    let own_run = add_class(
        &mut store,
        "probe/OwnRun",
        Some(thread),
        pool(&[]),
        vec![body()],
    );
    let task = add_class(&mut store, "probe/Task", None, pool(&[]), vec![body()]);
    let sub_task = add_class(&mut store, "probe/SubTask", Some(task), pool(&[]), vec![]);
    let other = add_class(&mut store, "probe/Other", None, pool(&[]), vec![body()]);
    Fixture {
        store,
        thread,
        worker,
        own_run,
        task,
        sub_task,
        other,
    }
}

fn frame(class: &str, id: ClassId, method: &str) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: 0,
        class_id: Some(id),
        method_index: None,
    })
}

fn answer(
    fx: &Fixture,
    outermost: &BacktraceFrame,
    thread: ClassId,
    task: ClassId,
) -> Option<(String, String, i32, i32, Option<ClassId>)> {
    thread_run_standin_frame(&fx.store, outermost, thread, task).map(|f| {
        let e = f.to_entry();
        (
            e.class_name.to_string(),
            e.method_name.to_string(),
            e.line_number,
            e.byte_code_index,
            e.class_id,
        )
    })
}

#[test]
fn a_task_run_by_the_served_thread_run_gets_the_thread_run_frame() {
    let fx = fixture(jdk25_run());
    let want = Some((
        "java/lang/Thread".to_string(),
        "run".to_string(),
        1474,
        7,
        Some(fx.thread),
    ));
    let task_run = frame("probe/Task", fx.task, "run");
    assert!(may_be_thread_run_task_frame(&task_run));
    assert_eq!(answer(&fx, &task_run, fx.thread, fx.task), want);
    // A Thread subclass that inherits `run`, and a task inheriting its `run`.
    assert_eq!(answer(&fx, &task_run, fx.worker, fx.sub_task), want);
    // A compiled outermost activation names its class in its label.
    let compiled = BacktraceFrame::Compiled {
        label: Arc::from("probe/Task.run:()V"),
        owner_class_id: fx.task.as_u32(),
        bci: 0,
        cp_stamp: None,
    };
    assert_eq!(answer(&fx, &compiled, fx.thread, fx.task), want);    }

#[test]
fn no_thread_run_frame_where_hotspot_has_none() {
    let fx = fixture(jdk25_run());
    let task_run = frame("probe/Task", fx.task, "run");
    // The thread overrides `run`: the override is the bottom frame.
    assert_eq!(answer(&fx, &task_run, fx.own_run, fx.task), None);
    // The task's `run` is not the outermost frame's.
    assert_eq!(answer(&fx, &task_run, fx.thread, fx.other), None);
    // `Thread.run` itself ran as bytecode, or the frame is not `run()V`.
    let thread_run = frame("java/lang/Thread", fx.thread, "run");
    assert!(!may_be_thread_run_task_frame(&thread_run));
    assert_eq!(answer(&fx, &thread_run, fx.thread, fx.task), None);
    let call = frame("probe/Task", fx.task, "call");
    assert!(!may_be_thread_run_task_frame(&call));
    assert_eq!(answer(&fx, &call, fx.thread, fx.task), None);
}

#[test]
fn an_ambiguous_or_native_thread_run_gets_no_frame() {
    let two_calls = java(
        "run",
        "()V",
        vec![
            0x2a, 0x01, 0x01, 0xb7, 0, 6, 0x2a, 0x01, 0x01, 0xb7, 0, 6, 0xb1,
        ],
        &[(0, 1474)],
    );
    for run in [two_calls, native("run", "()V")] {
        let fx = fixture(run);
        let task_run = frame("probe/Task", fx.task, "run");
        assert_eq!(answer(&fx, &task_run, fx.thread, fx.task), None);
    }
}
