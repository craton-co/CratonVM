use super::test_support::{named_method, store_with};
use super::*;

/// Interpreter round i1 wave 34: only a JDK class or a hidden class can
/// have a hidden frame, so any other entry is decided without the class
/// manager.
#[test]
fn only_jdk_and_hidden_class_entries_may_be_hidden() {
    assert!(entry_may_be_hidden("java/lang/Thread"));
    assert!(entry_may_be_hidden("jdk/internal/vm/ScopedValueContainer"));
    assert!(entry_may_be_hidden("sun/nio/ch/Net"));
    assert!(entry_may_be_hidden(
        "com/example/Gen$Hid/0x0000000800c01000"
    ));
    assert!(!entry_may_be_hidden("com/example/Main"));
    assert!(!entry_may_be_hidden("Main"));
    assert!(!entry_may_be_hidden("javax/swing/JFrame"));
}

/// Interpreter round i1 wave 37: the label screen admits every label
/// whose class [`entry_may_be_hidden`] admits, without splitting it.
#[test]
fn the_label_screen_is_a_superset_of_the_class_screen() {
    for label in [
        "java/lang/ScopedValue$Carrier.runWith:(Ljava/lang/Runnable;)V",
        "jdk/internal/vm/Continuation.enter:()V",
        "sun/nio/ch/Net.poll:()I",
        "com/example/Gen$Hid/0x0000000800c01000.apply:(I)I",
    ] {
        let (class_name, _) = split_frame_label(label).expect("parses");
        assert!(entry_may_be_hidden(class_name), "{label}");
        assert!(label_may_be_hidden(label), "{label}");
    }
    assert!(!label_may_be_hidden(
        "com/example/Main.main:([Ljava/lang/String;)V"
    ));
    assert!(!label_may_be_hidden("javax/swing/JFrame.show:()V"));
}

/// `store_with`'s class is `ClassId(0)` in its fresh store, and an
/// inlined level reads a class id of `0` as "none recorded" (it keeps the
/// frame without asking). Re-add the class at the next, non-zero id so a
/// fixture's inlined level names it.
fn moved_to_a_nonzero_id(store: &mut ClassStore, id: ClassId) -> ClassId {
    let mut class = store.remove(id).expect("the fixture's class");
    class.id = store.next_id();
    let moved = store.add(class);
    assert_ne!(moved.as_u32(), 0);
    moved
}

/// Interpreter round i1 wave 37: a compiled activation and an inlined
/// level of a hidden class's method are hidden frames, as its interpreted
/// frame is (waves 34-35 kept them); an inlined level with no recorded
/// class id is kept, and a frame of an ordinary class is decided without
/// the store.
#[test]
fn compiled_and_inlined_frames_of_a_hidden_class_are_hidden() {
    let (mut store, id) = store_with(vec![named_method("apply", "(I)I", Vec::new())]);
    let id = moved_to_a_nonzero_id(&mut store, id);
    {
        let class = store.get_mut(id).expect("class");
        class.name = Arc::from("probe/W37Gen$Hid/0x10");
        class.hidden = true;
    }
    let label: Arc<str> = Arc::from("probe/W37Gen$Hid/0x10.apply:(I)I");
    let compiled = BacktraceFrame::Compiled {
        label: Arc::clone(&label),
        owner_class_id: id.as_u32(),
        bci: 1,
        cp_stamp: None,
    };
    let inlined = BacktraceFrame::Inlined {
        label: Arc::clone(&label),
        class_id: id.as_u32(),
        bci: 1,
        cp_stamp: None,
        hidden: None,
    };
    let unrecorded = BacktraceFrame::Inlined {
        label: Arc::clone(&label),
        class_id: 0,
        bci: 1,
        cp_stamp: None,
        hidden: None,
    };
    assert_eq!(backtrace_frame_hidden_decided(&compiled), None);
    assert_eq!(backtrace_frame_hidden_decided(&inlined), None);
    assert_eq!(backtrace_frame_hidden_decided(&unrecorded), Some(false));
    assert!(backtrace_frame_is_hidden(&store, &compiled));
    // The memoized answer is the same.
    assert!(backtrace_frame_is_hidden(&store, &compiled));
    assert!(backtrace_frame_is_hidden(&store, &inlined));
    assert!(!backtrace_frame_is_hidden(&store, &unrecorded));

    let plain = BacktraceFrame::Compiled {
        label: Arc::from("probe/W37Plain.apply:(I)I"),
        owner_class_id: id.as_u32(),
        bci: 1,
        cp_stamp: None,
    };
    assert_eq!(backtrace_frame_hidden_decided(&plain), Some(false));    }

/// Interpreter round i6 wave 118, lane L3 (`CRATONVM_HIDDEN_METHOD_BITS`):
/// the store's per-class bits answer what the per-method scan answers, by
/// slot and by name, and a change through `get_mut` (a redefinition's
/// route) is seen on the next ask.
#[test]
fn i118_l3_hidden_method_bits_answer_as_the_scan_and_follow_get_mut() {
    use cratonvm_reader::attribute::{Annotation, LazyAttribute};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let mut annotated = named_method("runWith", "(Ljava/lang/Runnable;)V", Vec::new());
    annotated.attributes.push(LazyAttribute::new_decoded(
        Attribute::RuntimeVisibleAnnotations(vec![Annotation {
            type_index: 1,
            element_value_pairs: Vec::new(),
        }]),
    ));
    let plain = named_method("runWith", "()V", Vec::new());
    let (mut store, id) = store_with(vec![plain, annotated]);
    let id = moved_to_a_nonzero_id(&mut store, id);
    {
        let class = store.get_mut(id).expect("class");
        class.name = Arc::from("java/lang/W118Carrier");
        class.loader_id = crate::classloading::ClassLoaderId::Bootstrap;
        class.constant_pool = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(cratonvm_reader::constant_pool::CpUtf8::from_arc(Arc::from("Ljdk/internal/vm/annotation/Hidden;"))),
        ]);
    }
    let ask = |store: &ClassStore, desc: Option<&str>, index: Option<u32>| {
        method_is_hidden(store, id, "runWith", desc, index)
    };
    let cases: [(Option<&str>, Option<u32>); 5] = [
        (Some("(Ljava/lang/Runnable;)V"), None),
        (Some("(Ljava/lang/Runnable;)V"), Some(1)),
        (Some("(Ljava/lang/Runnable;)V"), Some(0)),
        (Some("()V"), Some(0)),
        (None, None),
    ];
    let scanned: Vec<bool> = cases.iter().map(|(d, i)| ask(&store, *d, *i)).collect();
    store.set_hidden_method_bits_on(true);
    let by_bits: Vec<bool> = cases.iter().map(|(d, i)| ask(&store, *d, *i)).collect();
    assert_eq!(scanned, by_bits);
    assert_eq!(by_bits, [true, true, true, false, true]);
    let bits = store
        .hidden_method_bits(id, |_| panic!("kept: not recomputed"))
        .expect("bits");
    assert_eq!(&*bits, &[0b10u64]);
    // A user-loader class: the bits are recomputed after `get_mut`.
    store.get_mut(id).expect("class").loader_id =
        crate::classloading::ClassLoaderId::Application;
    if true {
        assert!(!ask(&store, Some("(Ljava/lang/Runnable;)V"), Some(1)));
    }
}

/// Interpreter round i6 wave 123, lane L3 (the cut half of step 3 of
/// `i37-L6-proposal-hidden-method-bit-at-link-time`): the windowed redo
/// answers exactly the whole redo's cap, for stacks shorter and longer
/// than the cap, sparse and dense hidden frames, and a window that must
/// grow more than once.
#[test]
fn i123_l3_recapture_past_hidden_cut_answers_the_whole_redo() {
    let frame = |bci: u32| BacktraceFrame::Inlined {
        label: Arc::from("p/Deep.f:()V"),
        class_id: 1,
        bci,
        cp_stamp: None,
        hidden: None,
    };
    let bci_of = |f: &BacktraceFrame| match f {
        BacktraceFrame::Inlined { bci, .. } => *bci,
        _ => u32::MAX,
    };
    for total in [0u32, 5, 99, 100, 101, 250, 1000, 5000] {
        for every in [1u32, 2, 3, 7, 50] {
            for max_depth in [1usize, 10, 100] {
                // Raw frames OUTERMOST first; a frame is hidden when its
                // number is a multiple of `every` (`1`: all hidden).
                let raw: Vec<u32> = (0..total).collect();
                let mut attempt = |window: usize| {
                    let start = if window == 0 {
                        0
                    } else {
                        raw.len().saturating_sub(window)
                    };
                    let taken = &raw[start..];
                    let kept: Vec<BacktraceFrame> = taken
                        .iter()
                        .filter(|n| **n % every != 0)
                        .map(|n| frame(*n))
                        .collect();
                    (kept, taken.len())
                };
                let (mut whole, _) = attempt(0);
                cap_throwable_trace(&mut whole, max_depth);
                let cut = raw.len().min(max_depth);
                let kept_in_cut = raw[raw.len() - cut..]
                    .iter()
                    .filter(|n| **n % every != 0)
                    .count();
                let windowed =
                    recapture_past_hidden_cut(max_depth, cut, kept_in_cut, &mut attempt);
                assert_eq!(
                    windowed.iter().map(bci_of).collect::<Vec<_>>(),
                    whole.iter().map(bci_of).collect::<Vec<_>>(),
                    "total={total} every={every} max_depth={max_depth}"
                );
            }
        }
    }
    assert!(hidden_cut_first_window(1024, 1024, 0) > 1024);
    assert!(hidden_cut_first_window(1024, 1024, 512) >= 2048);
    assert!(hidden_cut_first_window(usize::MAX, usize::MAX, 1) == usize::MAX);
}

/// Wave 123 (L3): the stamp is not part of a stand-in site (a stamped
/// and an unstamped splice of one callee stand at the same call), nor of
/// an unstamped level's screen key (tag 2, as before), but it is part of a
/// stamped one's.
#[test]
fn i123_l3_the_stamp_splits_no_standin_site_and_keys_the_screen() {
    let label: Arc<str> =
        Arc::from("java/lang/Thread.runWith:(Ljava/lang/Object;Ljava/lang/Runnable;)V");
    let level = |hidden: Option<bool>| BacktraceFrame::Inlined {
        label: Arc::clone(&label),
        class_id: 9,
        bci: 4,
        cp_stamp: None,
        hidden,
    };
    assert!(same_standin_site(&level(None), &level(Some(true))));
    assert!(same_standin_site(&level(Some(false)), &level(Some(true))));
    let key = |hidden| screen_frame_key(&level(hidden)).map(|k| k.0);
    assert_eq!(key(None), Some(2));
    assert_ne!(key(Some(true)), key(None));
    assert_ne!(key(Some(true)), key(Some(false)));
}

/// Interpreter round i6 wave 122, lane L2 (step 3 of
/// `i37-L6-proposal-hidden-method-bit-at-link-time`): the resolver's stamp
/// is `None` with the store's bits off and the scan's answer with them on;
/// a stamped inlined level is decided without a store, whatever its label
/// says, and an unstamped one is judged by its label as before.
#[test]
fn i122_l2_inline_callee_hidden_stamp_decides_an_inlined_level() {
    use cratonvm_reader::attribute::{Annotation, LazyAttribute};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let mut annotated = named_method("runWith", "(Ljava/lang/Runnable;)V", Vec::new());
    annotated.attributes.push(LazyAttribute::new_decoded(
        Attribute::RuntimeVisibleAnnotations(vec![Annotation {
            type_index: 1,
            element_value_pairs: Vec::new(),
        }]),
    ));
    let plain = named_method("runWith", "()V", Vec::new());
    let (mut store, id) = store_with(vec![plain, annotated]);
    let id = moved_to_a_nonzero_id(&mut store, id);
    {
        let class = store.get_mut(id).expect("class");
        class.name = Arc::from("java/lang/W122Carrier");
        class.loader_id = crate::classloading::ClassLoaderId::Bootstrap;
        class.constant_pool = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(cratonvm_reader::constant_pool::CpUtf8::from_arc(Arc::from("Ljdk/internal/vm/annotation/Hidden;"))),
        ]);
    }
    let hidden_desc = "(Ljava/lang/Runnable;)V";
    store.set_hidden_method_bits_on(false);
    assert_eq!(inline_callee_hidden_stamp(&store, id, "runWith", hidden_desc), None);
    store.set_hidden_method_bits_on(true);
    assert_eq!(
        inline_callee_hidden_stamp(&store, id, "runWith", hidden_desc),
        Some(method_is_hidden(&store, id, "runWith", Some(hidden_desc), None))
    );
    assert_eq!(inline_callee_hidden_stamp(&store, id, "runWith", "()V"), Some(false));
    let level = |label: &str, hidden: Option<bool>| BacktraceFrame::Inlined {
        label: Arc::from(label),
        class_id: id.as_u32(),
        bci: 1,
        cp_stamp: None,
        hidden,
    };
    // A user label the screen would call visible: the stamp wins.
    let user = level("p/User.run:()V", Some(true));
    assert_eq!(backtrace_frame_hidden_decided(&user), Some(true));
    assert_eq!(backtrace_frame_hidden_decided_scanned(&user), Some(true));
    assert!(backtrace_frame_is_hidden(&store, &user));
    // A JDK label the screen leaves undecided: the stamp decides it.
    let jdk = level("java/lang/W122Carrier.runWith:()V", Some(false));
    assert_eq!(backtrace_frame_hidden_decided(&jdk), Some(false));
    assert!(!backtrace_frame_is_hidden(&store, &jdk));
    // Unstamped: the label screen, as before.
    let unstamped = level("java/lang/W122Carrier.runWith:(Ljava/lang/Runnable;)V", None);
    assert_eq!(backtrace_frame_hidden_decided(&unstamped), None);
}

/// Interpreter round i1 wave 37: an `@Hidden` JDK method is hidden as a
/// compiled activation, an inlined level and a walk's entry (with and
/// without its method index); its unannotated overload is not, and the
/// same annotation on a non-JDK class is ignored, as HotSpot honours it
/// only in privileged code.
#[test]
fn an_annotated_jdk_method_is_hidden_in_every_frame_shape() {
    use cratonvm_reader::attribute::{Annotation, LazyAttribute};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let mut annotated = named_method("runWith", "(Ljava/lang/Runnable;)V", Vec::new());
    annotated.attributes.push(LazyAttribute::new_decoded(
        Attribute::RuntimeVisibleAnnotations(vec![Annotation {
            type_index: 1,
            element_value_pairs: Vec::new(),
        }]),
    ));
    let plain = named_method("runWith", "()V", Vec::new());
    let (mut store, id) = store_with(vec![plain, annotated]);
    let id = moved_to_a_nonzero_id(&mut store, id);
    {
        let class = store.get_mut(id).expect("class");
        class.name = Arc::from("java/lang/W37Carrier");
        // Interpreter round i6 wave 115, lane L3: `@Hidden` counts only in
        // a class the boot or platform loader defined.
        class.loader_id = crate::classloading::ClassLoaderId::Bootstrap;
        class.constant_pool = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("Ljdk/internal/vm/annotation/Hidden;".into()),
        ]);
    }
    let label = "java/lang/W37Carrier.runWith:(Ljava/lang/Runnable;)V";
    let compiled = BacktraceFrame::Compiled {
        label: Arc::from(label),
        owner_class_id: id.as_u32(),
        bci: 0,
        cp_stamp: None,
    };
    let inlined = BacktraceFrame::Inlined {
        label: Arc::from(label),
        class_id: id.as_u32(),
        bci: 0,
        cp_stamp: None,
        hidden: None,
    };
    assert!(backtrace_frame_is_hidden(&store, &compiled));
    assert!(backtrace_frame_is_hidden(&store, &inlined));
    let overload = BacktraceFrame::Compiled {
        label: Arc::from("java/lang/W37Carrier.runWith:()V"),
        owner_class_id: id.as_u32(),
        bci: 0,
        cp_stamp: None,
    };
    assert!(!backtrace_frame_is_hidden(&store, &overload));

    let mut entry = synthetic_entry(Arc::from("java/lang/W37Carrier"), Arc::from("runWith"));
    entry.class_id = Some(id);
    entry.method_descriptor = Some(Arc::from("(Ljava/lang/Runnable;)V"));
    assert!(stack_entry_is_hidden(&store, &entry));
    entry.method_index = Some(1);
    assert!(stack_entry_is_hidden(&store, &entry));
    // A stale index (it names the overload) falls back to the scan.
    entry.method_index = Some(0);
    assert!(stack_entry_is_hidden(&store, &entry));

    // Interpreter round i6 wave 115, lane L3: a JDK-named class an
    // application loader defined keeps its annotated frame, as HotSpot's
    // `ClassFileParser` drops `@Hidden` outside privileged code.
    {
        let class = store.get_mut(id).expect("class");
        class.name = Arc::from("sun/example/W37Carrier");
        class.loader_id = crate::classloading::ClassLoaderId::Application;
    }
    let mut user = entry.clone();
    user.class_name = Arc::from("sun/example/W37Carrier");
    if true {
        assert!(!stack_entry_is_hidden(&store, &user));
    }
    store.get_mut(id).expect("class").loader_id =
        crate::classloading::ClassLoaderId::Bootstrap;
    assert!(stack_entry_is_hidden(&store, &user));

    store.get_mut(id).expect("class").name = Arc::from("com/example/W37Carrier");
    let mut app = entry.clone();
    app.class_name = Arc::from("com/example/W37Carrier");
    assert!(!stack_entry_is_hidden(&store, &app));
}
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LineNumberEntry};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;

/// A deferred entry with no `method_index` — i.e. what the lock-free
/// `capture_frames_no_lines` snapshot produces.
fn entry_for(class_id: ClassId, method: &str, bci: i32) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from("probe/Target"),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: Some(Arc::from("Target.java")),
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(class_id),
        method_index: None,
    }
}

/// A deferred entry that carries the method slot, i.e. what
/// `entry_from_frame` produces.
fn entry_for_indexed(
    class_id: ClassId,
    method: &str,
    bci: i32,
    method_index: u32,
) -> StackTraceEntry {
    StackTraceEntry {
        method_index: Some(method_index),
        ..entry_for(class_id, method, bci)
    }
}

/// One entry naming a program point, for the overlap tests.
fn point(class: &str, method: &str, bci: i32) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: None,
        method_index: None,
    }
}

/// The property the retired `may_need_throwable_fill_frame_trim` pre-check
/// existed for: `is_a` -- which takes the class-manager lock -- is asked
/// only for a frame whose NAME already matched, so an ordinary trace never
/// takes it. Plus the two phase rules: `fillInStackTrace` first, then
/// `<init>`, each a prefix from the innermost end, each stopped by the
/// first frame whose holder the throwable is not.
#[test]
fn fill_frame_trim_asks_is_a_only_for_a_matching_name() {
    let mut asked = 0usize;
    let plain = [point("P", "main", 1), point("P", "throwSite", 2)];
    let keep = throwable_fill_frame_keep_len(&plain, &mut |_, _| {
        asked += 1;
        true
    });
    assert_eq!((keep, asked), (2, 0));

    let built = [
        point("P", "main", 1),
        point("E", "<init>", 2),
        point("T", "<init>", 3),
        point("T", "fillInStackTrace", 4),
    ];
    assert_eq!(throwable_fill_frame_keep_len(&built, &mut |_, _| true), 1);

    // `fillInStackTrace` BELOW an `<init>` is past both prefixes.
    let reordered = [point("P", "fillInStackTrace", 1), point("E", "<init>", 2)];
    assert_eq!(
        throwable_fill_frame_keep_len(&reordered, &mut |_, _| true),
        1
    );

    // A constructor of a class the throwable is not stops the phase.
    let foreign = [point("P", "main", 1), point("Other", "<init>", 2)];
    assert_eq!(
        throwable_fill_frame_keep_len(&foreign, &mut |_, name| name != "Other"),
        2
    );
    assert_eq!(
        throwable_fill_frame_keep_len::<StackTraceEntry>(&[], &mut |_, _| true),
        0
    );
}

/// HotSpot `MaxJavaStackTraceDepth`: the cap keeps the INNERMOST entries,
/// which are the LAST ones of an outermost-first trace, and `0` is
/// unlimited.
#[test]
fn throwable_trace_cap_keeps_the_innermost_entries() {
    let trace: Vec<StackTraceEntry> = (0..10).map(|i| point("P", "m", i)).collect();
    let mut capped = trace.clone();
    cap_throwable_trace(&mut capped, 4);
    let bcis: Vec<i32> = capped.iter().map(|e| e.byte_code_index).collect();
    assert_eq!(
        bcis,
        vec![6, 7, 8, 9],
        "the throw site (last entry) survives"
    );

    let mut short = trace.clone();
    cap_throwable_trace(&mut short, 10);
    assert_eq!(short.len(), 10, "at the cap: untouched");
    let mut unlimited = trace;
    cap_throwable_trace(&mut unlimited, 0);
    assert_eq!(unlimited.len(), 10, "0 means unlimited");

    assert_eq!(MAX_JAVA_STACK_TRACE_DEPTH, 1024, "HotSpot's default");
    assert_eq!(throwable_capture_budget(0), usize::MAX);
    assert!(throwable_capture_budget(MAX_JAVA_STACK_TRACE_DEPTH) > MAX_JAVA_STACK_TRACE_DEPTH);
}

/// Interpreter frames `probe/Deep.m0 .. m{n-1}` (outermost first) followed
/// by `probe/Deep.<init>` x `inits`, each at a distinct bci.
fn deep_frames(n: usize, inits: usize) -> Vec<Frame> {
    (0..n + inits)
        .map(|i| {
            let name = if i < n {
                format!("m{i}")
            } else {
                "<init>".to_string()
            };
            let mut f = Frame::new(
                ClassId::new(1),
                "probe/Deep".to_string(),
                name,
                "()V".to_string(),
                None,
                vec![0xb1],
                Vec::new(),
                1,
                1,
                &[],
            );
            f.last_instr_pc = i;
            f
        })
        .collect()
}

/// The bounded capture (only `max_depth + slack` innermost interpreter
/// frames walked) gives exactly the unbounded capture's capped answer --
/// including when the fill-frame trim eats more than the slack and the
/// capture has to be redone unbounded.
#[test]
fn bounded_throwable_capture_matches_the_unbounded_one() {
    if !crate::jit::stack_trace::active_compiled_frames().is_empty() {
        // Compiled frames on this thread take the unbounded path anyway.
        return;
    }
    let bcis =
        |t: &[StackTraceEntry]| -> Vec<i32> { t.iter().map(|e| e.byte_code_index).collect() };
    // Holder class 1 is the throwable's class: every `<init>` is trimmed.
    let mut trim_all = |holder: Option<ClassId>, _: &str| holder == Some(ClassId::new(1));
    for (n, inits, max_depth) in [
        (300, 0, 10),
        (300, 3, 10),
        (300, 64, 10),  // the trim leaves exactly `max_depth` in the window
        (300, 65, 10),  // ... one fewer: redone unbounded
        (300, 100, 10), // the trim eats all of the 64-frame slack
        (20, 3, 10),    // shallower than the budget: no window
        (300, 3, 0),    // unlimited
    ] {
        let frames = deep_frames(n, inits);
        let mut full = capture_full_trace_without_store(&frames);
        full.truncate(n);
        cap_throwable_trace(&mut full, max_depth);
        let got = capture_throwable_trace_without_store(&frames, max_depth, &mut trim_all);
        assert_eq!(
            bcis(&got),
            bcis(&full),
            "n={n} inits={inits} max_depth={max_depth}"
        );
        let expect_len = if max_depth == 0 { n } else { n.min(max_depth) };
        assert_eq!(got.len(), expect_len);
        assert_eq!(got.last().map(|e| e.byte_code_index), Some(n as i32 - 1));
    }
}

/// Stage 1a of the compact-backtrace proposal: the retained form
/// ([`BacktraceFrame`]) materialises into exactly the entries the eager
/// capture builds, field for field, for cached and owned interpreter
/// frames alike, under the fill-frame trim and the depth cap.
#[test]
fn backtrace_capture_materialises_the_eager_entries() {
    if !crate::jit::stack_trace::active_compiled_frames().is_empty() {
        return;
    }
    let cached = Arc::new(
        crate::classloading::resolution::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(2),
                class_name: Arc::from("probe/Cached"),
                method_name: Arc::from("hot"),
                method_descriptor: Arc::from("()V"),
                source_file: Some(Arc::from("Cached.java")),
                code: Arc::from(vec![0xb1u8]),
                exception_table: Arc::from(Vec::new()),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ),
    );
    // 40 named frames and 3 trailing `<init>`s of the throwable's class
    // (trimmed); every third named frame runs the cached method.
    let mut frames = deep_frames(40, 3);
    for i in (0..40).step_by(3) {
        let mut f = Frame::new_pooled_cached(
            Arc::clone(&cached),
            &[],
            &mut Vec::new(),
            &mut Vec::new(),
        );
        f.last_instr_pc = 1000 + i;
        frames[i] = f;
    }
    let key = |e: &StackTraceEntry| {
        (
            e.class_name.to_string(),
            e.method_name.to_string(),
            e.method_descriptor.as_deref().map(str::to_string),
            e.source_file.as_deref().map(str::to_string),
            e.line_number,
            e.byte_code_index,
            e.class_id,
            e.method_index,
        )
    };
    let mut trim_all = |holder: Option<ClassId>, _: &str| holder == Some(ClassId::new(1));
    for max_depth in [0, 10, 1024] {
        let eager = capture_throwable_trace_without_store(&frames, max_depth, &mut trim_all);
        let compact =
            capture_throwable_backtrace_without_store(&frames, max_depth, &mut trim_all, &[]);
        assert!(
            compact
                .iter()
                .any(|f| matches!(f, BacktraceFrame::Method { .. })),
            "cached frames must be retained compactly (max_depth={max_depth})"
        );
        let got: Vec<_> = compact.iter().map(|f| key(&f.to_entry())).collect();
        let want: Vec<_> = eager.iter().map(key).collect();
        assert_eq!(got, want, "max_depth={max_depth}");
    }
}

/// The shape that motivated [`trailing_overlap`]: the throwable was built
/// while the compiled frames were still live, so the capture already spliced
/// them and the snapshot repeats all but the innermost.
#[test]
fn a_snapshot_that_repeats_the_captured_tail_is_appended_only_once() {
    let trace = vec![
        point("P", "main", 66),
        point("P", "probe", 42),
        point("P", "outer", 27),
        point("P", "mid", 26),
    ];
    let fresh = vec![
        point("P", "probe", 42),
        point("P", "outer", 27),
        point("P", "mid", 26),
        point("P", "leaf", 25),
    ];
    assert_eq!(trailing_overlap(&trace, &fresh), 3);
}

/// The ORDINARY shape — the frames had unwound, so nothing repeats and
/// everything is appended. This is the arm that must stay byte-for-byte
/// what it was.
#[test]
fn a_snapshot_of_frames_that_already_unwound_overlaps_nothing() {
    let trace = vec![point("P", "msg", 1), point("P", "pair", 48)];
    let fresh = vec![point("P", "probe", 42), point("P", "leaf", 25)];
    assert_eq!(trailing_overlap(&trace, &fresh), 0);
}

/// Genuine recursion appears twice in BOTH lists, and the MAXIMAL overlap
/// is what lines them up. A membership test would delete a real frame here.
#[test]
fn genuine_recursion_keeps_every_copy() {
    let trace = vec![
        point("P", "main", 3),
        point("P", "rec", 7),
        point("P", "rec", 7),
    ];
    let fresh = vec![
        point("P", "rec", 7),
        point("P", "rec", 7),
        point("P", "leaf", 1),
    ];
    // Two `rec` frames are shared; the third entry of `fresh` is new.
    assert_eq!(trailing_overlap(&trace, &fresh), 2);
}

/// A frame that matches by NAME but stands at a different bci is a
/// different program point and must not be folded away.
#[test]
fn a_different_bci_is_a_different_program_point() {
    let trace = vec![point("P", "main", 3), point("P", "rec", 7)];
    let fresh = vec![point("P", "rec", 9), point("P", "leaf", 1)];
    assert_eq!(trailing_overlap(&trace, &fresh), 0);
}

/// An empty snapshot, and an empty trace, both overlap nothing rather than
/// dividing by zero on the way there.
#[test]
fn an_empty_side_overlaps_nothing() {
    let some = vec![point("P", "m", 0)];
    assert_eq!(trailing_overlap(&some, &[]), 0);
    assert_eq!(trailing_overlap(&[], &some), 0);
}

/// WALK21-9 / WALK21-10: a walk window needs the capture's pair rules
/// exactly when one of its entries passes a rule's own callee screen.
#[test]
fn r21_walk_tail_pair_rule_screen() {
    let entry = |method: &str| StackTraceEntry {
        class_name: Arc::from("p/C"),
        method_name: Arc::from(method),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: -1,
        byte_code_index: 0,
        class_id: None,
        method_index: None,
    };
    assert!(walk_tail_needs_no_pair_rule(&[entry("deepA"), entry("stepA")]));
    assert!(walk_tail_needs_no_pair_rule(&[]));
    for callee in ["<clinit>", "loadClass", "findClass"] {
        assert!(!walk_tail_needs_no_pair_rule(&[entry("deepA"), entry(callee)]), "{callee}");
    }
    let run = entry("run");
    assert_eq!(
        walk_tail_needs_no_pair_rule(&[run.clone()]),
        !may_be_thread_run_middle_callee_name("run")
    );
}

/// WALK21-8 (JIT round 21 wave 1, lane walk): the per-thread method-slot
/// front cache answers only the key it holds, a colliding key replaces
/// it, and `clear_method_slot_memo` empties it.
#[test]
fn r21_walk_method_slot_front_cache_answers_only_its_key() {
    clear_method_slot_memo();
    let a = (7u32, signature_hash("m", "()V"));
    let b = (8u32, signature_hash("m", "()V"));
    assert_eq!(method_slot_front_get(a), None);
    method_slot_front_put(a, 3);
    assert_eq!(method_slot_front_get(a), Some(3));
    assert_eq!(method_slot_front_get(b), None);
    // A key that maps to the same entry evicts it.
    let collide = (0..10_000u32)
        .map(|c| (c, a.1))
        .find(|k| *k != a && method_slot_front_index(*k) == method_slot_front_index(a))
        .expect("some class id shares the entry");
    method_slot_front_put(collide, 5);
    assert_eq!(method_slot_front_get(collide), Some(5));
    assert_eq!(method_slot_front_get(a), None);
    clear_method_slot_memo();
    assert_eq!(method_slot_front_get(collide), None);
    assert!(method_slot_front_index(a) < METHOD_SLOT_FRONT_ENTRIES);
}

// -- memoized find_method ------------------------------------------

#[test]
fn memo_cold_and_warm_agree_and_pick_the_right_overload() {
    let (store, cid) = store_with(vec![
        named_method(
            "run",
            "(I)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 11,
            }],
        ),
        named_method(
            "run",
            "(J)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 22,
            }],
        ),
        named_method(
            "other",
            "()V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 33,
            }],
        ),
    ]);

    clear_method_slot_memo();
    // Cold (memo empty → linear scan) …
    assert_eq!(line_number_for_bci(&store, cid, "run", "(I)V", 0), Some(11));
    assert_eq!(line_number_for_bci(&store, cid, "run", "(J)V", 0), Some(22));
    assert_eq!(
        line_number_for_bci(&store, cid, "other", "()V", 0),
        Some(33)
    );
    // … and warm (memo populated → verified index hit) must agree exactly.
    assert_eq!(line_number_for_bci(&store, cid, "run", "(I)V", 0), Some(11));
    assert_eq!(line_number_for_bci(&store, cid, "run", "(J)V", 0), Some(22));
    assert_eq!(
        line_number_for_bci(&store, cid, "other", "()V", 0),
        Some(33)
    );
}

#[test]
fn memo_never_holds_a_negative() {
    let (store, cid) = store_with(vec![named_method("present", "()V", Vec::new())]);
    clear_method_slot_memo();
    // Absent method: must not be memoized (a redefinition may add it).
    assert_eq!(line_number_for_bci(&store, cid, "absent", "()V", 0), None);
    assert!(
        !method_slot_memo()
            .read()
            .contains_key(&(cid.as_u32(), signature_hash("absent", "()V"))),
        "a failed lookup must never be memoized"
    );
}

#[test]
fn memo_self_heals_when_the_class_is_redefined_under_it() {
    // Populate the memo against a 2-method class, then rebuild the store
    // with the methods in the opposite order (what a redefinition looks
    // like from this module's point of view) and re-query. The stored
    // index is now wrong; verification must catch it.
    clear_method_slot_memo();
    let (store, cid) = store_with(vec![
        named_method(
            "a",
            "()V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 1,
            }],
        ),
        named_method(
            "b",
            "()V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 2,
            }],
        ),
    ]);
    assert_eq!(line_number_for_bci(&store, cid, "a", "()V", 0), Some(1));
    assert_eq!(line_number_for_bci(&store, cid, "b", "()V", 0), Some(2));

    let (store2, cid2) = store_with(vec![
        named_method(
            "b",
            "()V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 20,
            }],
        ),
        named_method(
            "a",
            "()V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 10,
            }],
        ),
    ]);
    assert_eq!(cid2, cid, "fixture builds both stores at slot 0");
    assert_eq!(line_number_for_bci(&store2, cid2, "a", "()V", 0), Some(10));
    assert_eq!(line_number_for_bci(&store2, cid2, "b", "()V", 0), Some(20));
}

#[test]
fn unloaded_class_resolves_to_none_not_to_a_neighbour() {
    clear_method_slot_memo();
    let (mut store, cid) = store_with(vec![named_method(
        "run",
        "()V",
        vec![LineNumberEntry {
            start_pc: 0,
            line_number: 7,
        }],
    )]);
    assert_eq!(line_number_for_bci(&store, cid, "run", "()V", 0), Some(7));
    // Unload: ClassStore leaves a tombstone and never reuses the ClassId,
    // so a deferred resolution against a stale id fails closed.
    let _ = store.remove(cid);
    assert_eq!(line_number_for_bci(&store, cid, "run", "()V", 0), None);
}

// -- deferred (lazy) line resolution --------------------------------

#[test]
fn deferred_resolution_fills_unambiguous_frames() {
    clear_method_slot_memo();
    let (store, cid) = store_with(vec![named_method(
        "compute",
        "()I",
        vec![
            LineNumberEntry {
                start_pc: 0,
                line_number: 40,
            },
            LineNumberEntry {
                start_pc: 4,
                line_number: 41,
            },
        ],
    )]);
    let mut entries = vec![entry_for(cid, "compute", 0), entry_for(cid, "compute", 6)];
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 2);
    assert_eq!(entries[0].line_number, 40);
    assert_eq!(entries[1].line_number, 41);
}

#[test]
fn deferred_resolution_fails_closed_on_overloads() {
    clear_method_slot_memo();
    let (store, cid) = store_with(vec![
        named_method(
            "run",
            "(I)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 11,
            }],
        ),
        named_method(
            "run",
            "(J)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 22,
            }],
        ),
    ]);
    let mut entries = vec![entry_for(cid, "run", 0)];
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
    assert_eq!(
        entries[0].line_number, LINE_NUMBER_UNKNOWN,
        "an overload set must stay UNKNOWN rather than guess a body"
    );
}

// -- exact deferred resolution via `method_index` (CR-SW-1) ----------

/// Two overloads with different `LineNumberTable`s. Without an index the
/// resolver must decline (covered above); with one it must pick the exact
/// body — this is the whole point of the field.
fn overloaded_store() -> (ClassStore, ClassId) {
    store_with(vec![
        named_method(
            "run",
            "(I)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 11,
            }],
        ),
        named_method(
            "run",
            "(J)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 22,
            }],
        ),
        named_method(
            "run",
            "(Ljava/lang/String;)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 33,
            }],
        ),
    ])
}

#[test]
fn deferred_resolution_is_exact_for_overloads_when_the_index_is_present() {
    clear_method_slot_memo();
    let (store, cid) = overloaded_store();
    let mut entries = vec![
        entry_for_indexed(cid, "run", 0, 0),
        entry_for_indexed(cid, "run", 0, 1),
        entry_for_indexed(cid, "run", 0, 2),
    ];
    assert_eq!(
        resolve_line_numbers_in_place(&store, &mut entries),
        3,
        "every overloaded frame must resolve once the slot is carried"
    );
    assert_eq!(entries[0].line_number, 11);
    assert_eq!(entries[1].line_number, 22);
    assert_eq!(entries[2].line_number, 33);
}

#[test]
fn deferred_resolution_with_index_matches_the_eager_answer_exactly() {
    // The contract that lets a future pass defer the Throwable path: for
    // every overload, deferred-with-index == eager.
    clear_method_slot_memo();
    let (store, cid) = overloaded_store();
    for (idx, descriptor) in [(0u32, "(I)V"), (1, "(J)V"), (2, "(Ljava/lang/String;)V")] {
        let eager = line_number_for_bci(&store, cid, "run", descriptor, 0);
        let mut entries = vec![entry_for_indexed(cid, "run", 0, idx)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
        assert_eq!(
            Some(entries[0].line_number),
            eager,
            "deferred resolution of slot {idx} must equal the eager answer"
        );
    }
}

#[test]
fn a_stale_index_falls_back_rather_than_resolving_the_wrong_body() {
    // A redefinition reorders methods under a captured trace. The index now
    // names a *different* method, so the name check must reject it. Here
    // the name is unique after the reorder, so the fallback resolves it
    // correctly; the point is that the wrong body is never used.
    clear_method_slot_memo();
    let (store, cid) = store_with(vec![
        named_method(
            "other",
            "()V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 99,
            }],
        ),
        named_method(
            "compute",
            "()I",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 40,
            }],
        ),
    ]);
    // Entry captured when `compute` was at slot 0.
    let mut entries = vec![entry_for_indexed(cid, "compute", 0, 0)];
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
    assert_eq!(
        entries[0].line_number, 40,
        "must not report `other`'s line 99"
    );
}

#[test]
fn a_stale_index_into_an_overload_set_fails_closed() {
    // Same as above but the fallback cannot help: the name is overloaded.
    // The only acceptable answer is UNKNOWN.
    clear_method_slot_memo();
    let (store, cid) = overloaded_store();
    let mut entries = vec![entry_for_indexed(cid, "missing", 0, 1)];
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
    assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);

    // And an out-of-range index (class shrank under the trace).
    let mut entries = vec![entry_for_indexed(cid, "run", 0, 99)];
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
    assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);
}

#[test]
fn an_index_pointing_at_the_wrong_overload_is_still_rejected_by_name_only_if_names_differ() {
    // Honest statement of the residual: the verification is by *name*, so a
    // stale index that happens to land on ANOTHER member of the same
    // overload set passes the check. That is why the index is written at
    // capture time and only ever read against the same live class — and why
    // the Throwable path stays eager rather than deferring across a window
    // in which a redefinition could reorder an overload set.
    clear_method_slot_memo();
    let (store, cid) = overloaded_store();
    let mut entries = vec![entry_for_indexed(cid, "run", 0, 1)];
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
    assert_eq!(entries[0].line_number, 22, "slot 1 is `run(J)V`");
}

#[test]
fn resolution_never_touches_an_entry_whose_class_is_gone_even_with_an_index() {
    clear_method_slot_memo();
    let (mut store, cid) = overloaded_store();
    let mut entries = vec![entry_for_indexed(cid, "run", 0, 0)];
    let _ = store.remove(cid);
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
    assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);
}

#[test]
fn synthetic_and_lockfree_entries_carry_no_method_index() {
    let e = synthetic_entry(Arc::from("java/lang/Object"), Arc::from("hashCode"));
    assert!(e.method_index.is_none());
    // `capture_frames_no_lines` is covered by its own integration path; the
    // invariant asserted here is the one the resolver depends on.
    assert_eq!(entry_for(ClassId::new(0), "run", 0).method_index, None);
}

#[test]
fn deferred_resolution_never_overwrites_an_already_resolved_or_native_frame() {
    clear_method_slot_memo();
    let (store, cid) = store_with(vec![named_method(
        "compute",
        "()I",
        vec![LineNumberEntry {
            start_pc: 0,
            line_number: 40,
        }],
    )]);
    let mut entries = vec![entry_for(cid, "compute", 0), entry_for(cid, "compute", 0)];
    entries[0].line_number = 999; // already resolved eagerly
    entries[1].line_number = LINE_NUMBER_NATIVE;
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
    assert_eq!(entries[0].line_number, 999);
    assert_eq!(entries[1].line_number, LINE_NUMBER_NATIVE);
}

#[test]
fn deferred_resolution_after_unload_is_unknown_not_wrong() {
    clear_method_slot_memo();
    let (mut store, cid) = store_with(vec![named_method(
        "compute",
        "()I",
        vec![LineNumberEntry {
            start_pc: 0,
            line_number: 40,
        }],
    )]);
    let mut entries = vec![entry_for(cid, "compute", 0)];
    let _ = store.remove(cid);
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
    assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);
    // Frame identity survives the unload — the trace still names the frame.
    assert_eq!(&*entries[0].class_name, "probe/Target");
    assert_eq!(&*entries[0].method_name, "compute");
    assert_eq!(entries[0].byte_code_index, 0);
}

#[test]
fn deferred_resolution_skips_synthetic_entries() {
    clear_method_slot_memo();
    let (store, _cid) = store_with(vec![named_method("compute", "()I", Vec::new())]);
    let mut entries = vec![synthetic_entry(
        Arc::from("java/lang/Object"),
        Arc::from("hashCode"),
    )];
    assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
    assert_eq!(entries[0].line_number, LINE_NUMBER_NATIVE);
}

#[test]
fn signature_hash_separates_name_and_descriptor_domains() {
    assert_ne!(signature_hash("ab", "()V"), signature_hash("a", "b()V"));
    assert_eq!(signature_hash("run", "()V"), signature_hash("run", "()V"));
}

fn make_method_with_line_table(entries: Vec<LineNumberEntry>) -> ClassFileMethod {
    let code = CodeAttribute {
        max_stack: 1,
        max_locals: 1,
        code: cratonvm_reader::ByteView::from_vec(vec![0xb1]), // return
        exception_table: Vec::new(),
        attributes: vec![Attribute::LineNumberTable(entries)],
    };
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from("probe"),
        descriptor: Arc::from("()V"),
        attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
            Attribute::Code(Box::new(code)),
        )],
    }
}

#[test]
fn line_table_picks_largest_start_leq_bci() {
    let method = make_method_with_line_table(vec![
        LineNumberEntry {
            start_pc: 0,
            line_number: 10,
        },
        LineNumberEntry {
            start_pc: 5,
            line_number: 20,
        },
        LineNumberEntry {
            start_pc: 9,
            line_number: 30,
        },
    ]);
    // Reimplement the core scan logic here against the method directly
    // (the `line_number_for_bci` entry point requires a ClassStore and
    // is covered by integration tests in vm/tests/wp1_9_*).
    let code = method.code().unwrap();
    let lnt = match &code.attributes[0] {
        Attribute::LineNumberTable(e) => e,
        _ => panic!(),
    };

    let lookup = |bci: u16| -> Option<u16> {
        let mut best = None;
        let mut best_start = 0;
        for e in lnt {
            if e.start_pc <= bci && (best.is_none() || e.start_pc >= best_start) {
                best_start = e.start_pc;
                best = Some(e.line_number);
            }
        }
        best
    };

    assert_eq!(lookup(0), Some(10));
    assert_eq!(lookup(3), Some(10));
    assert_eq!(lookup(5), Some(20));
    assert_eq!(lookup(8), Some(20));
    assert_eq!(lookup(9), Some(30));
    assert_eq!(lookup(42), Some(30));
}

#[test]
fn line_table_multiple_attrs_combined() {
    // A method with two LineNumberTable attributes (permitted by spec).
    let code = CodeAttribute {
        max_stack: 1,
        max_locals: 1,
        code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
        exception_table: Vec::new(),
        attributes: vec![
            Attribute::LineNumberTable(vec![LineNumberEntry {
                start_pc: 0,
                line_number: 5,
            }]),
            Attribute::LineNumberTable(vec![LineNumberEntry {
                start_pc: 10,
                line_number: 99,
            }]),
        ],
    };
    let method = ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from("probe"),
        descriptor: Arc::from("()V"),
        attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
            Attribute::Code(Box::new(code)),
        )],
    };
    let code = method.code().unwrap();

    // Simulate the union semantics `line_number_for_bci` uses.
    let mut best = None;
    let mut best_start = 0;
    for attr in &code.attributes {
        if let Attribute::LineNumberTable(entries) = attr {
            for e in entries {
                if e.start_pc <= 12 && (best.is_none() || e.start_pc >= best_start) {
                    best_start = e.start_pc;
                    best = Some(e.line_number);
                }
            }
        }
    }
    assert_eq!(best, Some(99));
}

#[test]
fn synthetic_entry_has_native_sentinel() {
    let e = synthetic_entry(Arc::from("java/lang/Object"), Arc::from("hashCode"));
    assert_eq!(e.line_number, LINE_NUMBER_NATIVE);
    assert_eq!(e.byte_code_index, -1);
    assert!(e.source_file.is_none());
}

#[test]
fn unknown_is_minus_one() {
    assert_eq!(LINE_NUMBER_UNKNOWN, -1);
}

#[test]
fn native_is_minus_two() {
    assert_eq!(LINE_NUMBER_NATIVE, -2);
}

// -- the trap snapshot: still-live frames and the OSR override ------

fn interp_frame(method: &str, class_id: ClassId, last_instr_pc: usize) -> Frame {
    let mut f = Frame::new(
        class_id,
        "probe/Target".to_string(),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0; 128],
        vec![],
        4,
        4,
        &[],
    );
    f.last_instr_pc = last_instr_pc;
    f
}

fn compiled_at(interp_depth: u32, method: &str, bci: i32) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth,
        chain_index: 0,
        label: Arc::from(format!("probe/Target.{method}:()V")),
        owner_class_id: 0,
        cm_ptr: 0,
        bci,
        inline_chain: Vec::new(),
        chain_exact: false,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: None,
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

fn captured(class_id: ClassId, method: &str, bci: i32) -> StackTraceEntry {
    StackTraceEntry {
        method_descriptor: Some(Arc::from("()V")),
        ..entry_for(class_id, method, bci)
    }
}

/// A compiled frame an interpreter frame still sits above cannot have
/// unwound, so the late capture already walked it; only the frames above
/// the innermost interpreter frame are the snapshot's to recover. The
/// `--real-jdk` + `CRATONVM_BG_COMPILE=0` shape: `main` compiled at depth 0
/// under two interpreted callers, `leaf` compiled above them.
#[test]
fn a_compiled_frame_under_a_live_interpreter_frame_is_not_spliced_again() {
    let cid = ClassId::new(1);
    let frames = vec![interp_frame("probe", cid, 7), interp_frame("outer", cid, 1)];
    let kept = drop_still_live(
        &frames,
        vec![compiled_at(0, "main", 69), compiled_at(2, "leaf", 13)],
    );
    let names: Vec<&str> = kept.iter().map(|f| &*f.label).collect();
    assert_eq!(names, vec!["probe/Target.leaf:()V"]);
}

/// Every frame at or above the innermost interpreter frame is kept -- which
/// includes an OSR continuation of that frame, recorded at exactly
/// `frames.len()`, so it still reaches the override.
#[test]
fn frames_that_unwound_are_all_kept() {
    let cid = ClassId::new(1);
    let frames = vec![interp_frame("main", cid, 84)];
    let kept = drop_still_live(
        &frames,
        vec![compiled_at(1, "main", 130), compiled_at(1, "probe", 7)],
    );
    assert_eq!(kept.len(), 2);
}

/// The override re-points the entry of ITS frame and re-resolves the line.
/// Two activations of one method at one bci: the in-order walk must pick
/// the second entry for frame 1, not the first entry that matches by name.
#[test]
fn an_osr_override_repoints_its_own_frames_entry() {
    let (store, cid) = store_with(vec![named_method(
        "rec",
        "()V",
        vec![
            LineNumberEntry {
                start_pc: 0,
                line_number: 10,
            },
            LineNumberEntry {
                start_pc: 50,
                line_number: 20,
            },
        ],
    )]);
    let frames = vec![interp_frame("rec", cid, 7), interp_frame("rec", cid, 7)];
    let mut trace = vec![captured(cid, "rec", 7), captured(cid, "rec", 7)];
    let mut overrides = OsrBciOverrides::default();
    overrides.insert(1, (60, Vec::new()));
    apply_snapshot_osr_overrides(&store, &frames, &overrides, &mut trace);
    assert_eq!(trace[0].byte_code_index, 7);
    assert_eq!(trace[1].byte_code_index, 60);
    assert_eq!(trace[1].line_number, 20);
}

/// The stack-walker residual-costs page, item 3: a run of compiled
/// activations of one body at one bci (a recursion) clones the first
/// one's entry, and every entry is the one `compiled_frame_entry` builds;
/// another bci or another body builds afresh.
#[test]
fn a_repeated_compiled_activation_clones_the_entry_it_would_build() {
    let (store, cid) = store_with(vec![named_method(
        "rec",
        "()V",
        vec![
            LineNumberEntry {
                start_pc: 0,
                line_number: 10,
            },
            LineNumberEntry {
                start_pc: 50,
                line_number: 20,
            },
        ],
    )]);
    let label: Arc<str> = Arc::from("probe/Target.rec:()V");
    let at = |bci: i32, label: &Arc<str>| ActiveCompiledFrame {
        label: Arc::clone(label),
        owner_class_id: cid.as_u32(),
        ..compiled_at(0, "rec", bci)
    };
    let other: Arc<str> = Arc::from("probe/Target.rec:()V");
    let jit = [at(7, &label), at(7, &label), at(60, &label), at(60, &label), at(7, &other)];
    for resolve_lines in [true, false] {
        let mut repeat = CompiledEntryRepeat {
            last: None,
            on: true,
        };
        let mut memo = Vec::new();
        let mut fresh = Vec::new();
        for slot in &jit {
            push_compiled_frames_repeating(&mut memo, &store, slot, resolve_lines, &mut repeat);
            push_compiled_frames(&mut fresh, &store, slot, resolve_lines);
        }
        assert_eq!(memo.len(), jit.len());
        for (m, f) in memo.iter().zip(&fresh) {
            assert_eq!(
                (&m.class_name, &m.method_name, &m.method_descriptor, &m.source_file),
                (&f.class_name, &f.method_name, &f.method_descriptor, &f.source_file)
            );
            assert_eq!(
                (m.line_number, m.byte_code_index, m.class_id, m.method_index),
                (f.line_number, f.byte_code_index, f.class_id, f.method_index)
            );
        }
        if resolve_lines {
            let lines: Vec<i32> = memo.iter().map(|e| e.line_number).collect();
            assert_eq!(lines, vec![10, 10, 20, 20, 10]);
        }
    }
}

/// A frame whose entry is not in the capture ends the walk and leaves the
/// trace exactly as it was: a stale line, never a moved one.
#[test]
fn an_override_with_no_matching_entry_changes_nothing() {
    let (store, cid) = store_with(vec![named_method("rec", "()V", Vec::new())]);
    let frames = vec![interp_frame("rec", cid, 7)];
    let mut trace = vec![captured(cid, "rec", 9)];
    let mut overrides = OsrBciOverrides::default();
    overrides.insert(0, (60, Vec::new()));
    apply_snapshot_osr_overrides(&store, &frames, &overrides, &mut trace);
    assert_eq!(trace.len(), 1);
    assert_eq!(trace[0].byte_code_index, 9);
    assert_eq!(trace[0].line_number, LINE_NUMBER_UNKNOWN);
}

// -- round 12 wave 4 (exc2): the armed trap snapshot (W3-2) ------------

fn names_and_bcis(trace: &[StackTraceEntry]) -> Vec<(String, i32)> {
    trace
        .iter()
        .map(|e| (e.method_name.to_string(), e.byte_code_index))
        .collect()
}

/// While armed, a throwable capture lays the snapshot out as its compiled
/// frames -- below the constructor frames, which the fill-frame trim then
/// removes -- and a disarmed capture walks as before.
#[test]
fn an_armed_trap_snapshot_stands_in_for_the_compiled_walk() {
    let cid = ClassId::new(1);
    let base = vec![interp_frame("main", cid, 3), interp_frame("outer", cid, 5)];
    let snapshot = vec![compiled_at(2, "catcher", 11), compiled_at(2, "leaf", 13)];
    assert!(trap_snapshot_can_stand_in_for_walk(&base, &snapshot));
    let building = vec![
        interp_frame("main", cid, 3),
        interp_frame("outer", cid, 5),
        interp_frame("<init>", cid, 0),
        interp_frame("fillInStackTrace", cid, 0),
    ];
    let mut is_a = |_: Option<ClassId>, _: &str| true;

    let window = arm_trap_capture(snapshot, base.len()).expect("nothing armed yet");
    // A second door cannot arm over the first.
    assert!(arm_trap_capture(vec![compiled_at(2, "other", 1)], base.len()).is_err());
    // A capture below the armed depth is not the one being built.
    let shallow = capture_throwable_trace_without_store(&base[..1], 0, &mut is_a);
    assert_eq!(names_and_bcis(&shallow), vec![("main".to_string(), 3)]);
    let whole = capture_throwable_trace_without_store(&building, 0, &mut is_a);
    assert_eq!(
        names_and_bcis(&whole),
        vec![
            ("main".to_string(), 3),
            ("outer".to_string(), 5),
            ("catcher".to_string(), 11),
            ("leaf".to_string(), 13),
        ]
    );
    let (back, used) = window.disarm();
    assert!(used);
    assert_eq!(back.map(|f| f.len()), Some(2));

    let walked = capture_throwable_trace_without_store(&building, 0, &mut is_a);
    assert_eq!(
        names_and_bcis(&walked),
        vec![("main".to_string(), 3), ("outer".to_string(), 5)]
    );
}

/// JIT round 20 (lane throw): with the window's owner named, a capture of
/// another class takes the snapshot out for its duration (it walks), the
/// owner's own capture still takes it, and the guard puts it back.
#[test]
fn jr20_a_foreign_capture_inside_an_armed_window_walks_its_own_frames() {
    let cid = ClassId::new(1);
    let base = vec![interp_frame("main", cid, 3)];
    let building = vec![interp_frame("main", cid, 3), interp_frame("<init>", cid, 0)];
    let mut is_a = |_: Option<ClassId>, _: &str| false;
    let owner = ClassId::new(41);
    let foreign = ClassId::new(42);

    let window = arm_trap_capture(vec![compiled_at(1, "leaf", 13)], base.len()).expect("armed");
    // Unknown owner: every capture is the owner's (the old answer).
    assert!(suspend_trap_capture_for_foreign(foreign).is_none());
    bind_trap_capture_owner(owner);
    assert!(suspend_trap_capture_for_foreign(owner).is_none());
    {
        let _guard = suspend_trap_capture_for_foreign(foreign).expect("suspended");
        assert!(!trap_capture_armed());
        let walked = capture_throwable_trace_without_store(&building, 0, &mut is_a);
        assert!(names_and_bcis(&walked)
            .iter()
            .all(|(name, _)| name != "leaf"));
    }
    assert!(trap_capture_armed());
    let (back, used) = window.disarm();
    assert!(!used, "the foreign capture must not mark the snapshot used");
    assert_eq!(back.map(|f| f.len()), Some(1));
    assert!(!trap_capture_armed());
}

/// Round 12 wave 5 (lane exc3), W4-3: compiled and inlined frames are
/// retained as their shared label and materialise into exactly the entries
/// the eager capture builds -- labelled, inlined (with and without a
/// recorded class id), line-less (`bci == -1`) -- while an unparsable label
/// and an out-of-bound inlined bci are dropped by both forms alike.
#[test]
fn compiled_frames_are_retained_compactly_and_materialise_identically() {
    let cid = ClassId::new(1);
    let base = vec![interp_frame("main", cid, 3)];
    let level = |label: &str, bci: u32, class_id: u32| InlinedLevel {
        label: Arc::from(label),
        bci,
        class_id,
        hidden: None,
    };
    let mut spliced = compiled_at(1, "catcher", 11);
    spliced.owner_class_id = 7;
    // Innermost first, as the walk carries it.
    spliced.inline_chain = vec![
        level("probe/Target.inner:(I)I", 4, 0),
        level("probe/Other.mid:()V", 9, 5),
    ];
    let mut lineless = compiled_at(1, "noline", -1);
    lineless.owner_class_id = 7;
    // Innermost first: `far` (out of bound) sits BELOW `deep`, so the
    // chain keeps `deep` and ends at `far`.
    let mut cut = compiled_at(1, "cut", 2);
    cut.inline_chain = vec![
        level("probe/Target.far:()V", 70_000, 0),
        level("probe/Target.deep:()V", 1, 0),
    ];
    let garbled = ActiveCompiledFrame {
        label: Arc::from("no-separators"),
        ..compiled_at(1, "x", 1)
    };
    let snapshot = vec![spliced, lineless, cut, garbled, compiled_at(1, "leaf", 13)];
    let building = vec![
        interp_frame("main", cid, 3),
        interp_frame("<init>", cid, 0),
        interp_frame("fillInStackTrace", cid, 0),
    ];
    let key = |e: &StackTraceEntry| {
        (
            e.class_name.to_string(),
            e.method_name.to_string(),
            e.method_descriptor.as_deref().map(str::to_string),
            e.source_file.as_deref().map(str::to_string),
            e.line_number,
            e.byte_code_index,
            e.class_id,
            e.method_index,
        )
    };
    let mut is_a = |_: Option<ClassId>, _: &str| true;
    let window = arm_trap_capture(snapshot.clone(), base.len()).expect("nothing armed yet");
    let eager = capture_throwable_trace_without_store(&building, 0, &mut is_a);
    drop(window);
    let window = arm_trap_capture(snapshot, base.len()).expect("the first window closed");
    let compact = capture_throwable_backtrace_without_store(&building, 0, &mut is_a, &[]);
    drop(window);
    if compact_compiled_backtrace_enabled() {
        assert!(compact
            .iter()
            .any(|f| matches!(f, BacktraceFrame::Compiled { .. })));
        assert!(compact
            .iter()
            .any(|f| matches!(f, BacktraceFrame::Inlined { .. })));
    }
    let got: Vec<_> = compact.iter().map(|f| key(&f.to_entry())).collect();
    let want: Vec<_> = eager.iter().map(key).collect();
    assert_eq!(got, want);
    // main, catcher, mid, inner, noline, cut, deep (far cuts the chain),
    // leaf; the garbled label yields nothing.
    assert_eq!(
        eager
            .iter()
            .map(|e| e.method_name.to_string())
            .collect::<Vec<_>>(),
        vec!["main", "catcher", "mid", "inner", "noline", "cut", "deep", "leaf"]
    );
}

/// Dropping the window disarms, so a construction that unwound by panic
/// cannot hand its snapshot to the next capture on the thread.
#[test]
fn a_dropped_trap_capture_window_disarms() {
    let cid = ClassId::new(1);
    let base = vec![interp_frame("main", cid, 3)];
    {
        let _window = arm_trap_capture(vec![compiled_at(1, "leaf", 13)], base.len())
            .expect("nothing armed yet");
    }
    let mut is_a = |_: Option<ClassId>, _: &str| true;
    let walked = capture_throwable_trace_without_store(&base, 0, &mut is_a);
    assert_eq!(names_and_bcis(&walked), vec![("main".to_string(), 3)]);
    let window = arm_trap_capture(vec![compiled_at(1, "leaf", 13)], base.len())
        .expect("the dropped window disarmed");
    let (back, used) = window.disarm();
    assert!(!used);
    assert_eq!(back.map(|f| f.len()), Some(1));
}

/// Only a snapshot lying wholly above the innermost interpreter frame can
/// replace the walk.
#[test]
fn a_snapshot_under_or_past_the_interpreter_top_keeps_the_splice() {
    let cid = ClassId::new(1);
    let base = vec![interp_frame("main", cid, 3), interp_frame("outer", cid, 5)];
    assert!(!trap_snapshot_can_stand_in_for_walk(&base, &[]));
    assert!(!trap_snapshot_can_stand_in_for_walk(
        &base,
        &[compiled_at(1, "under", 4), compiled_at(2, "leaf", 13)]
    ));
    assert!(!trap_snapshot_can_stand_in_for_walk(
        &base,
        &[compiled_at(3, "past", 4)]
    ));
}

/// Interpreter round i1 wave 44, lane L3: a record kept outside the JIT
/// entry chain (a reflective call, a JNI native's JVMTI row) goes right
/// before the first slot pushed after it -- above the compiled
/// activations at its depth that were entered before it (chain index
/// below the chain length it recorded), below those entered after it
/// and below the interpreter frame it pushed.
#[test]
fn a_record_outside_the_chain_goes_between_the_activations_entered_before_and_after_it() {
    let cid = ClassId::new(1);
    let entered = |depth: u32, method: &str, chain_index: u32| ActiveCompiledFrame {
        chain_index,
        ..compiled_at(depth, method, 1)
    };
    // main, a (interpreted); `caller` compiled from a (chain entry 0),
    // the record made in it (chain length 1), `target` compiled after it
    // (entry 1) at the same depth, then `b` interpreted above them.
    let frames = vec![
        interp_frame("main", cid, 0),
        interp_frame("a", cid, 0),
        interp_frame("b", cid, 0),
    ];
    let jit = vec![entered(2, "caller", 0), entered(2, "target", 1)];
    let no_osr = OsrBciOverrides::default();
    let slots = collect_trace_slots(&frames, &jit, &no_osr);
    let name = |i: usize| slots[i].build().map(|e| e.method_name.to_string());
    let order: Vec<Option<String>> = (0..slots.len()).map(name).collect();
    assert_eq!(
        order,
        ["main", "a", "caller", "target", "b"]
            .map(|n| Some(n.to_string()))
            .to_vec()
    );
    assert_eq!(
        trace_anchor_position(&slots, 2, 1),
        Some(3),
        "above caller, below target"
    );
    assert_eq!(
        trace_anchor_position(&slots, 2, 0),
        Some(2),
        "below both activations"
    );
    assert_eq!(
        trace_anchor_position(&slots, 2, 2),
        Some(4),
        "entered after both: below b"
    );
    assert_eq!(
        trace_anchor_position(&slots, 1, 0),
        Some(1),
        "made in main: below a"
    );
    assert_eq!(
        trace_anchor_position(&slots, 3, 2),
        None,
        "innermost: nothing above"
    );
    // Without `b`: the target is the only thing the record's call pushed.
    let slots = collect_trace_slots(&frames[..2], &jit, &no_osr);
    assert_eq!(trace_anchor_position(&slots, 2, 1), Some(3));
    assert_eq!(trace_anchor_position(&slots, 2, 2), None);
}
/// Interpreter round i1 wave 45, lane L3: a published trace lists a
/// reflective call's JDK frames from the thread's memo alone, at the
/// placement the thread's own capture uses, and shifts every interpreter
/// frame's published position past them; frames memoized at another
/// redefinition count are not listed.
#[test]
fn a_published_trace_lists_the_memoized_reflective_frames_and_shifts_the_positions() {
    let cid = ClassId::new(1);
    // main -> (Method.invoke) -> target -> park
    let frames = vec![
        interp_frame("main", cid, 0),
        interp_frame("target", cid, 0),
        interp_frame("park", cid, 0),
    ];
    let calls = [ReflectiveCallRow {
        interp_depth: 1,
        jit_depth: 0,
        kind: cratonvm_native_api::ReflectiveCallKind::MethodInvoke,
        check: None,
        raised: None,
        door_save: None,
        adapter_pin: None,
    }];
    let jdk: Arc<[StackTraceEntry]> = Arc::from(vec![
        entry_for(cid, "invoke", 7),
        entry_for(cid, "accessorInvoke", 9),
    ]);
    let mut named: ReflectiveFramesNamed = Default::default();
    named[reflective_kind_slot(calls[0].kind)] = Some((5, Some(jdk)));
    let no_osr = OsrBciOverrides::default();
    let publish = |count: u64| {
        let mut trace = PublishedTrace {
            entries: capture_frames_no_lines(&frames),
            frame_positions: None,
            compiled_holds: Vec::new(),
        };
        splice_published_reflective_frames(
            &frames,
            &[],
            &no_osr,
            &calls,
            &named,
            &ServedThreadRunCalls::default(),
            count,
            &mut trace,
            None,
        );
        trace
    };
    let trace = publish(5);
    let order: Vec<&str> = trace.entries.iter().map(|e| &*e.method_name).collect();
    assert_eq!(
        order,
        ["main", "invoke", "accessorInvoke", "target", "park"]
    );
    assert_eq!(trace.frame_positions.as_deref(), Some(&[0u32, 3, 4][..]));
    // Named at another count: stale, nothing listed, positions untouched.
    let stale = publish(6);
    assert_eq!(stale.entries.len(), 3);
    assert_eq!(stale.frame_positions, None);
    // A call that pushed nothing yet (parked in the native itself).
    let mut trace = PublishedTrace {
        entries: capture_frames_no_lines(&frames[..1]),
        frame_positions: None,
        compiled_holds: Vec::new(),
    };
    splice_published_reflective_frames(
        &frames[..1],
        &[],
        &no_osr,
        &calls,
        &named,
        &ServedThreadRunCalls::default(),
        5,
        &mut trace,
        None,
    );
    assert_eq!(trace.entries.len(), 1);
}

/// Interpreter round i1 wave 45, lane L3: a throwable filled inside a
/// reflective call with nothing of the call above it -- its first pushed
/// frame is the first one the fill-frame trim removed, or it pushed none
/// -- lists the call's frames on top: its `raise` frames when it has
/// them, else its frames at their call lines. A call past `keep` still
/// lists nothing.
#[test]
fn a_throwable_raised_by_a_reflective_native_lists_the_call_on_top() {
    let cid = ClassId::new(1);
    let frames = vec![
        interp_frame("main", cid, 0),
        interp_frame("<init>", cid, 0),
        interp_frame("fillInStackTrace", cid, 0),
    ];
    let no_osr = OsrBciOverrides::default();
    let slots = collect_trace_slots(&frames, &[], &no_osr);
    let splice = |raise: Option<&str>| ReflectiveSplice {
        interp_depth: 1,
        jit_depth: 0,
        entries: Arc::from(vec![
            entry_for(cid, "invoke", 7),
            entry_for(cid, "accessorInvoke", 9),
        ]),
        raise: raise
            .map(|m| Arc::from(vec![entry_for(cid, "invoke", 7), entry_for(cid, m, 30)])),
    };
    let names = |built: Vec<StackTraceEntry>| -> Vec<String> {
        built.iter().map(|e| e.method_name.to_string()).collect()
    };
    // keep = 1: `<init>` and `fillInStackTrace` trimmed; the call's first
    // pushed slot is slot 1.
    let splices = [splice(Some("accessorRaise"))];
    let positions = reflective_splice_positions(&slots, &splices, true);
    assert_eq!(positions, vec![(1, 0)]);
    let built: Vec<StackTraceEntry> = build_slots_with_splices(&slots, 1, &splices, &positions);
    assert_eq!(names(built), ["main", "invoke", "accessorRaise"]);
    let splices = [splice(None)];
    let built: Vec<StackTraceEntry> = build_slots_with_splices(&slots, 1, &splices, &positions);
    assert_eq!(names(built), ["main", "invoke", "accessorInvoke"]);
    // Inside the trimmed frames (the call was made by `<init>`): nothing.
    let deeper = [ReflectiveSplice {
        interp_depth: 2,
        ..splice(None)
    }];
    let positions = reflective_splice_positions(&slots, &deeper, true);
    let built: Vec<StackTraceEntry> = build_slots_with_splices(&slots, 1, &deeper, &positions);
    assert_eq!(names(built), ["main"]);
    // Nothing pushed at all: on top of an untrimmed capture; the walks
    // (`unanchored_on_top == false`) still leave it out.
    let top = [ReflectiveSplice {
        interp_depth: 3,
        ..splice(Some("accessorRaise"))
    }];
    let positions = reflective_splice_positions(&slots, &top, true);
    assert_eq!(positions, vec![(3, 0)]);
    let built: Vec<StackTraceEntry> = build_slots_with_splices(&slots, 3, &top, &positions);
    assert_eq!(
        names(built),
        [
            "main",
            "<init>",
            "fillInStackTrace",
            "invoke",
            "accessorRaise"
        ]
    );
    assert!(reflective_splice_positions(&slots, &top, false).is_empty());
}

/// Round 15 wave 3 (lane trace4; TR2W2-2): served `Thread.run` rows go
/// right below the first frame their call pushed, merged with a
/// reflective call's frames by depth; `new W(new Thread(task)).run()`
/// gets two `Thread.run` frames (both calls stand at the same depth). The
/// walk reports where they went in. Failed before: no rows existed and
/// the capture listed `taskRun | wRun`.
#[test]
fn served_thread_run_rows_go_below_the_frames_their_calls_pushed() {
    let cid = ClassId::new(1);
    let frames = vec![
        interp_frame("main", cid, 0),
        interp_frame("wRun", cid, 0),
        interp_frame("taskRun", cid, 0),
    ];
    let mut calls = ServedThreadRunCalls {
        enabled: Some(true),
        ..Default::default()
    };
    let outer = calls.enter(|| (2, 0));
    let _inner = calls.enter(|| (2, 0));
    let rows = calls.splices(|| Some(entry_for(cid, "threadRun", 7)));
    assert_eq!(rows.len(), 2);
    let mut splices = vec![ReflectiveSplice {
        interp_depth: 1,
        jit_depth: 0,
        entries: Arc::from(vec![entry_for(cid, "invoke", 3)]),
        raise: None,
    }];
    merge_served_thread_run_splices(&mut splices, rows);
    let no_osr = OsrBciOverrides::default();
    let slots = collect_trace_slots(&frames, &[], &no_osr);
    let positions = reflective_splice_positions(&slots, &splices, false);
    assert_eq!(positions, vec![(1, 0), (2, 1), (2, 2)]);
    let built: Vec<StackTraceEntry> =
        build_slots_with_splices(&slots, slots.len(), &splices, &positions);
    let order: Vec<String> = built.iter().map(|e| e.method_name.to_string()).collect();
    assert_eq!(
        order,
        [
            "main",
            "invoke",
            "wRun",
            "threadRun",
            "threadRun",
            "taskRun"
        ]
    );
    // The walk form: entries as captured, and where each went in.
    let mut trace: Vec<StackTraceEntry> = slots.iter().filter_map(TraceSlot::build).collect();
    let inserted = splice_frames_reporting(&frames, &mut trace, &splices);
    assert_eq!(inserted, Some(vec![1, 2, 2]));
    assert_eq!(trace.len(), 6);
    assert_eq!(&*trace[3].method_name, "threadRun");
    // A trace that is not one entry per slot is left alone.
    let mut short = vec![entry_for(cid, "main", 0)];
    assert_eq!(splice_frames_reporting(&frames, &mut short, &splices), None);
    assert_eq!(short.len(), 1);
    // Leaving the outer call ends both.
    calls.leave(outer);
    assert!(calls.is_empty());
    assert!(calls
        .splices(|| Some(entry_for(cid, "threadRun", 7)))
        .is_empty());
    // The switch off records nothing.
    let mut off = ServedThreadRunCalls {
        enabled: Some(false),
        ..Default::default()
    };
    let mark = off.enter(|| (1, 0));
    assert_eq!(mark, 0);
    assert!(off.is_empty());
    // A frame that cannot be named lists nothing (the rules then answer).
    let mut unnamed = ServedThreadRunCalls {
        enabled: Some(true),
        ..Default::default()
    };
    unnamed.enter(|| (1, 0));
    assert!(unnamed.splices_unmemoized(|| None).is_empty());
}

/// Round 15 wave 6 (lane trace6; TR4W3-2 and the yield half of TR4W3-5):
/// the published trace lists the rows from the memo alone, once a call
/// named it, and a call left by a yield makes every reader fall back to
/// the rules. Failed before: there was no published form, and a remounted
/// thread's newer call listed only its own row.
#[test]
fn served_rows_publish_from_the_memo_and_stop_after_a_yield() {
    let cid = ClassId::new(1);
    let mut calls = ServedThreadRunCalls {
        enabled: Some(true),
        ..Default::default()
    };
    let count = cratonvm_classloading::class_redefinition_count();
    let outer = calls.enter(|| (1, 0));
    // Not named yet: a publish names nothing itself.
    assert!(calls.published_splices(count).is_empty());
    // A busy lock keeps the memo as it was.
    calls.name_if_stale(|| None);
    assert!(calls.published_splices(count).is_empty());
    calls.name_if_stale(|| Some(Some(entry_for(cid, "threadRun", 7))));
    let rows = calls.published_splices(count);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].interp_depth, 1);
    assert_eq!(&*rows[0].entries[0].method_name, "threadRun");
    // Named at another redefinition count: stale, not listed.
    assert!(calls.published_splices(count.wrapping_add(1)).is_empty());
    // A current memo is not named again.
    let mut asked = false;
    calls.name_if_stale(|| {
        asked = true;
        None
    });
    assert!(!asked);
    // The task yielded inside the call: its row is gone, and a newer call
    // must not speak for the thread alone.
    calls.leave_yielded(outer);
    let _newer = calls.enter(|| (2, 0));
    assert!(!calls.is_empty());
    assert!(calls.published_splices(count).is_empty());
    assert!(calls
        .splices(|| Some(entry_for(cid, "threadRun", 7)))
        .is_empty());
    assert!(calls
        .splices_unmemoized(|| Some(entry_for(cid, "threadRun", 7)))
        .is_empty());
}

/// Round 15 wave 3 hand-back (lane trace4): only `java.lang.Thread.runWith`
/// is the `--compatible` capture's hidden frame.
#[test]
fn only_thread_run_with_is_the_compatible_hidden_frame() {
    let frame = |class: &str, method: &str| {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from(class),
            ..entry_for(ClassId::new(1), method, 0)
        })
    };
    assert!(is_thread_run_with_frame(&frame(
        "java/lang/Thread",
        "runWith"
    )));
    assert!(!is_thread_run_with_frame(&frame("java/lang/Thread", "run")));
    assert!(!is_thread_run_with_frame(&frame(
        "java/lang/ScopedValue$Carrier",
        "runWith"
    )));
    assert!(!is_thread_run_with_frame(&frame("probe/Target", "runWith")));
}

/// Round 15 wave 3 (lane trace4): a throwable capture keeps its bounded
/// window when every splice stands below it (a started thread's bottom
/// `Thread.run` row), with the same answer the full capture gives.
#[test]
fn splices_below_the_window_keep_the_bounded_capture() {
    let cid = ClassId::new(1);
    let frames: Vec<Frame> = (0..100)
        .map(|i| interp_frame(&format!("f{i}"), cid, 0))
        .collect();
    let row_at = |interp_depth: u32| ReflectiveSplice {
        interp_depth,
        jit_depth: 0,
        entries: Arc::from(vec![entry_for(cid, "threadRun", 7)]),
        raise: None,
    };
    let names = |trace: Vec<StackTraceEntry>| -> Vec<String> {
        trace.iter().map(|e| e.method_name.to_string()).collect()
    };
    let mut never = |_: Option<ClassId>, _: &str| false;
    let bottom = [row_at(0)];
    let capped: Vec<StackTraceEntry> = capture_throwable_slots(&frames, 4, &mut never, &bottom);
    assert_eq!(names(capped), ["f96", "f97", "f98", "f99"]);
    // Uncapped, the row is there, below `f0`.
    let whole: Vec<StackTraceEntry> = capture_throwable_slots(&frames, 0, &mut never, &bottom);
    assert_eq!(whole.len(), 101);
    assert_eq!(&*whole[0].method_name, "threadRun");
    // A splice inside the window still goes in.
    let inner = [row_at(98)];
    let capped: Vec<StackTraceEntry> = capture_throwable_slots(&frames, 4, &mut never, &inner);
    assert_eq!(names(capped), ["f97", "threadRun", "f98", "f99"]);
}
