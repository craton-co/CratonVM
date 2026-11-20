use super::r13_trace3_standin_tests::{add_class, java, pool};
use super::*;

struct Fixture {
    store: ClassStore,
    thread: ClassId,
    base: ClassId,
    own_run: ClassId,
    caller: ClassId,
    task: ClassId,
}

/// `Thread` with JDK 25's `run()` (`runWith` at pc 7, line 1474); `Base
/// extends Thread` without `run`; `OwnRun extends Thread` with its own;
/// `Task`; and a `Caller` whose methods call, at bci 1 (0 for the
/// static): `invokespecial Base.run` (javac's `super.run()` in a
/// subclass of `Base`), `invokevirtual Thread.run`, `invokespecial
/// OwnRun.run`, `invokestatic Thread.run`.
fn fixture() -> Fixture {
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
        vec![java(
            "run",
            "()V",
            vec![0xb8, 0, 12, 0x4c, 0x2a, 0x2b, 0x2b, 0xb7, 0, 6, 0xb1],
            &[(0, 1472), (4, 1474)],
        )],
    );
    let body = || java("run", "()V", vec![0xb1], &[(0, 10)]);
    let base = add_class(&mut store, "probe/Base", Some(thread), pool(&[]), vec![]);
    let own_run = add_class(
        &mut store,
        "probe/OwnRun",
        Some(thread),
        pool(&[]),
        vec![body()],
    );
    let task = add_class(&mut store, "probe/Task", None, pool(&[]), vec![body()]);
    let caller = add_class(
        &mut store,
        "probe/Caller",
        None,
        pool(&[
            ("probe/Base", "run", "()V"),
            ("java/lang/Thread", "run", "()V"),
            ("probe/OwnRun", "run", "()V"),
            ("java/lang/Thread", "run", "()V"),
        ]),
        vec![
            java("viaSuper", "()V", vec![0x2a, 0xb7, 0, 6, 0xb1], &[(0, 20)]),
            java(
                "viaVirtual",
                "()V",
                vec![0x2a, 0xb6, 0, 12, 0xb1],
                &[(0, 30)],
            ),
            java("viaOwn", "()V", vec![0x2a, 0xb7, 0, 18, 0xb1], &[(0, 40)]),
            java("viaStatic", "()V", vec![0xb8, 0, 24, 0xb1], &[(0, 50)]),
        ],
    );
    Fixture {
        store,
        thread,
        base,
        own_run,
        caller,
        task,
    }
}

fn at(class: &str, id: ClassId, method: &str, bci: i32) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(id),
        method_index: None,
    })
}

fn answer(
    fx: &Fixture,
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
) -> Option<(String, String, i32, i32, Option<ClassId>)> {
    let names = [
        ("java/lang/Thread", fx.thread),
        ("probe/Base", fx.base),
        ("probe/OwnRun", fx.own_run),
    ];
    let mut resolve =
        |_: ClassId, name: &str| names.iter().find(|(n, _)| *n == name).map(|(_, id)| *id);
    assert!(may_need_thread_run_middle_frame(caller, callee));
    thread_run_middle_frame(&fx.store, caller, callee, &mut resolve).map(|e| {
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
fn a_call_that_entered_the_served_thread_run_gets_the_middle_frame() {
    let fx = fixture();
    let want = Some((
        "java/lang/Thread".to_string(),
        "run".to_string(),
        1474,
        7,
        Some(fx.thread),
    ));
    let task_run = at("probe/Task", fx.task, "run", 0);
    // `super.run()` through a class that inherits `Thread.run`.
    let via_super = at("probe/Caller", fx.caller, "viaSuper", 1);
    assert_eq!(answer(&fx, &via_super, &task_run), want);
    // A direct `thread.run()` whose callee is a task, not an override.
    let via_virtual = at("probe/Caller", fx.caller, "viaVirtual", 1);
    assert_eq!(answer(&fx, &via_virtual, &task_run), want);
    // `invokespecial` is exact even when the task is itself a `Thread`.
    let own_run = at("probe/OwnRun", fx.own_run, "run", 0);
    assert_eq!(answer(&fx, &via_super, &own_run), want);
}

#[test]
fn a_lambda_tasks_implementation_method_gets_the_bottom_frame() {
    let fx = fixture();
    let lambda_body = || {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from("probe/Caller"),
            method_name: Arc::from("lambda$main$0"),
            method_descriptor: Some(Arc::from("()V")),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: 0,
            class_id: Some(fx.caller),
            method_index: None,
        })
    };
    let run = ("run", "()V");
    let body = ("probe/Caller", "lambda$main$0", "()V");
    let frame = lambda_body();
    assert!(may_be_thread_run_lambda_frame(&frame));
    let got = thread_run_standin_frame_for_lambda(&fx.store, &frame, fx.base, run, body)
        .map(|f| f.to_entry());
    let got = got.map(|e| (e.class_name.to_string(), e.line_number, e.class_id));
    assert_eq!(
        got,
        Some(("java/lang/Thread".to_string(), 1474, Some(fx.thread)))
    );
    // Not a `Runnable`, another method, or a thread overriding `run`.
    let answer = |sam, body, thread| {
        thread_run_standin_frame_for_lambda(&fx.store, &frame, thread, sam, body)
    };
    assert!(answer(("call", "()Ljava/lang/Object;"), body, fx.base).is_none());
    assert!(answer(run, ("probe/Caller", "lambda$main$1", "()V"), fx.base).is_none());
    assert!(answer(run, ("probe/Other", "lambda$main$0", "()V"), fx.base).is_none());
    assert!(answer(run, body, fx.own_run).is_none());
    // The main thread's outermost frame is never screened in.
    let main = at("probe/Caller", fx.caller, "main", 0);
    assert!(!may_be_thread_run_lambda_frame(&main));
}

#[test]
fn no_middle_frame_where_hotspot_has_none() {
    let fx = fixture();
    let task_run = at("probe/Task", fx.task, "run", 0);
    // A virtual call whose callee extends `Thread` dispatched to the
    // receiver's override, not to `Thread.run`.
    let via_virtual = at("probe/Caller", fx.caller, "viaVirtual", 1);
    let own_run = at("probe/OwnRun", fx.own_run, "run", 0);
    assert_eq!(answer(&fx, &via_virtual, &own_run), None);
    // The call resolves to an override, or is not a virtual/special call.
    let via_own = at("probe/Caller", fx.caller, "viaOwn", 1);
    assert_eq!(answer(&fx, &via_own, &task_run), None);
    let via_static = at("probe/Caller", fx.caller, "viaStatic", 0);
    assert_eq!(answer(&fx, &via_static, &task_run), None);
    // The caller does not stand at the call.
    let off_site = at("probe/Caller", fx.caller, "viaSuper", 0);
    assert_eq!(answer(&fx, &off_site, &task_run), None);
    // `Thread.run` ran as bytecode: its own frame is the callee.
    let thread_run = at("java/lang/Thread", fx.thread, "run", 7);
    let via_super = at("probe/Caller", fx.caller, "viaSuper", 1);
    assert!(!may_need_thread_run_middle_frame(&via_super, &thread_run));
}

/// Round 15 wave 3 (lane trace3): a lambda task's body is the callee the
/// served `Thread.run` leaves (the proxy pushes no frame). Failed before:
/// the pair was refused by the `run()V` screen.
#[test]
fn a_lambda_tasks_body_under_the_served_thread_run_gets_the_middle_frame() {
    let fx = fixture();
    let want = Some((
        "java/lang/Thread".to_string(),
        "run".to_string(),
        1474,
        7,
        Some(fx.thread),
    ));
    let body = |class: &str, id: ClassId, descriptor: &str| {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from("lambda$main$0"),
            method_descriptor: Some(Arc::from(descriptor)),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: 0,
            class_id: Some(id),
            method_index: None,
        })
    };
    let lambda = body("probe/Caller", fx.caller, "(I)V");
    let via_super = at("probe/Caller", fx.caller, "viaSuper", 1);
    let via_virtual = at("probe/Caller", fx.caller, "viaVirtual", 1);
    assert_eq!(answer(&fx, &via_super, &lambda), want);
    assert_eq!(answer(&fx, &via_virtual, &lambda), want);
    // A body declared in a `Thread` subclass (`super(() -> ...)`) is not
    // an override of `run`: the virtual call still entered `Thread.run`.
    let in_thread = body("probe/Base", fx.base, "()V");
    assert_eq!(answer(&fx, &via_virtual, &in_thread), want);
    // Not a call of `Thread.run`: an override, or a static call.
    let via_own = at("probe/Caller", fx.caller, "viaOwn", 1);
    assert_eq!(answer(&fx, &via_own, &lambda), None);
    let via_static = at("probe/Caller", fx.caller, "viaStatic", 0);
    assert_eq!(answer(&fx, &via_static, &lambda), None);
    // Any other callee name stays off the class-manager lock.
    let named = at("probe/Caller", fx.caller, "work", 0);
    assert!(!may_need_thread_run_middle_frame(&via_super, &named));
    assert!(may_be_thread_run_middle_callee_name("run"));
    assert!(may_be_thread_run_middle_callee_name("lambda$main$0"));
    assert!(may_be_thread_run_middle_callee_name("f$anonfun$1"));
    assert!(!may_be_thread_run_middle_callee_name("work"));
}

/// Round 15 wave 3 (lane trace3): another thread's PUBLISHED stack has no
/// descriptors; the caller is the one method of its name calling
/// `run()V` at its bci. Failed before: `standin_call_site` refused it.
#[test]
fn a_published_caller_gets_the_middle_frame() {
    let fx = fixture();
    let published = |method: &str, bci: i32| {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from("probe/Caller"),
            method_name: Arc::from(method),
            method_descriptor: None,
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(fx.caller),
            method_index: None,
        })
    };
    let mut task_run = at("probe/Task", fx.task, "run", 0);
    if let BacktraceFrame::Entry(e) = &mut task_run {
        e.method_descriptor = None;
    }
    let got = answer(&fx, &published("viaSuper", 1), &task_run);
    assert_eq!(
        got.map(|(class, _, line, ..)| (class, line)),
        Some(("java/lang/Thread".to_string(), 1474))
    );
    assert_eq!(answer(&fx, &published("viaOwn", 1), &task_run), None);
    assert_eq!(answer(&fx, &published("viaStatic", 0), &task_run), None);
    // Not at the call.
    assert_eq!(answer(&fx, &published("viaSuper", 0), &task_run), None);
}

/// Round 15 wave 3 (lane trace3): which task classes enter
/// `java.lang.Thread`'s own `run` (the nested-`Thread`-task hop of the
/// bottom rule).
#[test]
fn only_a_platform_thread_inheriting_run_enters_thread_run() {
    let fx = fixture();
    assert!(thread_task_enters_thread_run(&fx.store, fx.thread));
    assert!(thread_task_enters_thread_run(&fx.store, fx.base));
    assert!(!thread_task_enters_thread_run(&fx.store, fx.own_run));
    assert!(!thread_task_enters_thread_run(&fx.store, fx.task));
    assert!(!thread_task_enters_thread_run(&fx.store, fx.caller));
    // A class id with no class (a lambda proxy's) enters nothing.
    assert!(!thread_task_enters_thread_run(
        &fx.store,
        ClassId::new(4_000)
    ));
}

/// A pair the lock-free screen lets through whose caller does not stand at
/// a `run()V` call is decided quiet from the caller's bytes alone (the
/// JUnit/ForkJoin lambda frames of a Netty test); a `run()V` call is
/// not, since resolving its owner depends on what is loaded.
#[test]
fn a_pair_whose_caller_is_not_at_a_run_call_is_decided_quiet() {
    let fx = fixture();
    let lambda = BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("probe/Caller"),
        method_name: Arc::from("lambda$main$0"),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: 0,
        class_id: Some(fx.caller),
        method_index: None,
    });
    let task_run = at("probe/Task", fx.task, "run", 0);
    let callers = [
        at("probe/Caller", fx.caller, "viaSuper", 1),
        at("probe/Caller", fx.caller, "viaVirtual", 1),
        at("probe/Caller", fx.caller, "viaOwn", 1),
        at("probe/Caller", fx.caller, "viaStatic", 0),
        // off the call, and past the end of the code
        at("probe/Caller", fx.caller, "viaSuper", 0),
        at("probe/Caller", fx.caller, "viaSuper", 99),
    ];
    // `viaSuper`/`viaVirtual`/`viaOwn` at their call all name `run()V`.
    let decided: Vec<bool> = callers
        .iter()
        .map(|c| thread_run_middle_pair_decided_quiet(&fx.store, c, &task_run))
        .collect();
    assert_eq!(decided, [false, false, false, true, true, true]);
    assert!(thread_run_middle_pair_decided_quiet(
        &fx.store,
        &callers[3],
        &lambda
    ));
    assert!(!thread_run_middle_pair_decided_quiet(
        &fx.store,
        &callers[1],
        &lambda
    ));
    // A caller whose class is not in the store is never decided.
    let unknown = at("probe/Gone", ClassId::new(4_000), "m", 1);
    assert!(!thread_run_middle_pair_decided_quiet(
        &fx.store, &unknown, &task_run
    ));
    // Whole traces: one undecided pair spoils it.
    assert!(thread_run_middle_pairs_decided_quiet(
        &fx.store,
        &[callers[3].clone(), task_run.clone()]
    ));
    assert!(!thread_run_middle_pairs_decided_quiet(
        &fx.store,
        &[callers[1].clone(), task_run]
    ));
    assert!(thread_run_middle_pairs_decided_quiet(&fx.store, &[]));
}

/// The property the capture memo relies on: a pair decided quiet gets no
/// frame from [`thread_run_middle_frame`], whatever the names resolve to.
#[test]
fn a_pair_decided_quiet_gets_no_middle_frame() {
    let fx = fixture();
    let names = [
        ("java/lang/Thread", fx.thread),
        ("probe/Base", fx.base),
        ("probe/OwnRun", fx.own_run),
    ];
    let mut resolve =
        |_: ClassId, name: &str| names.iter().find(|(n, _)| *n == name).map(|(_, id)| *id);
    let callees = [
        at("probe/Task", fx.task, "run", 0),
        at("probe/OwnRun", fx.own_run, "run", 0),
        at("probe/Caller", fx.caller, "lambda$main$0", 0),
    ];
    for method in ["viaSuper", "viaVirtual", "viaOwn", "viaStatic"] {
        for bci in -1..8 {
            let caller = at("probe/Caller", fx.caller, method, bci);
            for callee in &callees {
                if !may_need_thread_run_middle_frame(&caller, callee) {
                    continue;
                }
                if thread_run_middle_pair_decided_quiet(&fx.store, &caller, callee) {
                    assert!(
                        thread_run_middle_frame(&fx.store, &caller, callee, &mut resolve)
                            .is_none(),
                        "{method}@{bci}"
                    );
                }
            }
        }
    }
}
