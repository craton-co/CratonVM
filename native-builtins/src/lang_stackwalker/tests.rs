use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// JDK 25's `StackStreamFactory.isReflectionFrame` by name (interpreter
/// round i1 wave 43, lane L3): the reflection classes and accessor
/// implementations (a nested one included) are reflection frames; the
/// accessor interfaces, the factory and `Reflection` are not.
#[test]
fn reflection_frames_are_the_jdk_25_reflection_and_accessor_classes() {
    for yes in [
        "java/lang/reflect/Method",
        "java/lang/reflect/Constructor",
        "jdk/internal/reflect/DirectMethodHandleAccessor",
        "jdk/internal/reflect/DirectMethodHandleAccessor$NativeAccessor",
        "jdk/internal/reflect/DirectConstructorHandleAccessor",
        "jdk/internal/reflect/NativeMethodAccessorImpl",
        "jdk/internal/reflect/GeneratedMethodAccessor12",
    ] {
        assert!(is_reflection_frame_class(yes), "{yes}");
    }
    for no in [
        "java/lang/reflect/Field",
        "jdk/internal/reflect/MethodAccessor",
        "jdk/internal/reflect/MethodHandleAccessorFactory",
        "jdk/internal/reflect/Reflection",
        "p/DirectMethodHandleAccessor",
    ] {
        assert!(!is_reflection_frame_class(no), "{no}");
    }
}

/// t1misc (WALKTHROW21-3, `t1-RL-TLWALK`): one name slot per distinct
/// method-name text, numbered in order of first appearance; a frame
/// without a class id takes none; a name seen again (a recursion, or
/// alternating methods of one class) reuses its slot.
#[test]
fn t1misc_kit_name_keys_number_distinct_names_and_skip_classless_frames() {
    let names = ["run", "step", "run", "step", "run", "orphan", "tail"];
    let classes = [0, 0, 1, 0, 0, u32::MAX, 1];
    let (keys, distinct) = kit_name_keys(names.iter().copied(), &classes);
    assert_eq!(keys, vec![0, 1, 0, 1, 0, u32::MAX, 2]);
    assert_eq!(distinct, 3, "run, step, tail; the classless frame takes no slot");
    // A shorter class list ends the zip: frames past it are not covered.
    let (keys, distinct) = kit_name_keys(names.iter().copied(), &classes[..2]);
    assert_eq!(keys, vec![0, 1]);
    assert_eq!(distinct, 2);
    assert_eq!(kit_name_keys(std::iter::empty(), &[]), (Vec::new(), 0));
}

#[test]
fn retain_class_reference_bit_matches_jdk_25_class_frame_info_layout() {
    assert_eq!(SF_FLAG_RETAIN_CLASS_REF, 1 << 27);
    assert_eq!(SF_FLAG_RETAIN_CLASS_REF & 0x00ff_ffff, 0);
}

/// Interpreter round i2 wave 54, lane L3: the walk count rides in the
/// bits of `flags` no Java reads, and leaves every other bit alone.
#[test]
fn the_walk_count_keeps_out_of_the_bits_java_reads() {
    assert_eq!(SF_WALK_COUNT_LOW & 0x00ff_ffff, 0);
    assert_eq!(SF_WALK_COUNT_HIGH & 0x00ff_ffff, 0);
    assert_eq!(
        (SF_WALK_COUNT_LOW | SF_WALK_COUNT_HIGH) & SF_FLAG_RETAIN_CLASS_REF as u32,
        0
    );
    let base = SF_FLAG_RETAIN_CLASS_REF | MODIFIER_NATIVE;
    for count in [0u64, 1, 7, 8, 100, 127, 128, 129, 1_000_003] {
        let flags = sfi_flags_with_walk_count(base, count);
        assert_eq!(sfi_walk_count_of(flags), count % 128, "{count}");
        assert_eq!(flags & 0x00ff_ffff, MODIFIER_NATIVE, "{count}");
        assert_ne!(flags & SF_FLAG_RETAIN_CLASS_REF, 0, "{count}");
        // Rewriting replaces, never ORs into, the old count.
        assert_eq!(sfi_walk_count_of(sfi_flags_with_walk_count(flags, 5)), 5);
    }
}

/// Interpreter round i2 wave 55, lane L3: a walk's lines keep the count
/// in two words, then each of the first 128 frames' line and no-file bit,
/// and decode back to them.
#[test]
fn a_walks_lines_keep_each_frames_line_and_no_file_bit() {
    let entry = |line: i32, class: bool, file: bool| StackTraceEntry {
        class_name: std::sync::Arc::from("p/C"),
        method_name: std::sync::Arc::from("m"),
        method_descriptor: None,
        source_file: file.then(|| std::sync::Arc::from("C.java")),
        line_number: line,
        byte_code_index: 0,
        class_id: class.then(|| cratonvm_types::ClassId::new(9)),
        method_index: None,
    };
    let frames = vec![
        entry(12, true, true),
        entry(-1, true, false),
        entry(LINE_NUMBER_NATIVE, true, true),
        entry(65_535, false, false),
    ];
    let lines = walk_lines_for(&frames);
    assert_eq!(lines.len(), WALK_LINES_HEADER + frames.len());
    let decoded: Vec<(i32, bool)> = lines[WALK_LINES_HEADER..]
        .iter()
        .map(|&e| walk_line_decode(e))
        .collect();
    assert_eq!(
        decoded,
        vec![(12, false), (-1, true), (-2, false), (65_535, false)]
    );
    let many: Vec<StackTraceEntry> = (0..300).map(|i| entry(i, true, true)).collect();
    assert_eq!(
        walk_lines_for(&many).len(),
        WALK_LINES_HEADER + WALK_LINES_MAX_FRAMES
    );
    // The index rides where a wave-54 frame keeps its walk count.
    assert_eq!(sfi_walk_count_of(sfi_flags_with_walk_count(0, 127)), 127);
}

/// WALK21-1 (JIT round 21 wave 1, lane walk): a deep walk's lines are
/// chunks of 128 frames, each with the walk's one header; chunk 0 is the
/// single array of a shorter walk, and frame `i` sits in chunk `i / 128`
/// at index `i % 128`.
#[test]
fn r21_walk_deep_walk_lines_chunks_cover_every_frame() {
    let entry = |line: i32| StackTraceEntry {
        class_name: std::sync::Arc::from("p/C"),
        method_name: std::sync::Arc::from("m"),
        method_descriptor: None,
        source_file: Some(std::sync::Arc::from("C.java")),
        line_number: line,
        byte_code_index: 0,
        class_id: Some(cratonvm_types::ClassId::new(9)),
        method_index: None,
    };
    let frames: Vec<StackTraceEntry> = (0..300).map(entry).collect();
    let count: u64 = 0x1_0000_0005;
    let c0 = walk_lines_chunk_for(&frames, 0, count);
    let c1 = walk_lines_chunk_for(&frames, 1, count);
    let c2 = walk_lines_chunk_for(&frames, 2, count);
    assert_eq!(c0.len(), WALK_LINES_HEADER + WALK_LINES_MAX_FRAMES);
    assert_eq!(c1.len(), WALK_LINES_HEADER + WALK_LINES_MAX_FRAMES);
    assert_eq!(c2.len(), WALK_LINES_HEADER + (300 - 2 * WALK_LINES_MAX_FRAMES));
    assert!(walk_lines_chunk_for(&frames, 3, count).is_empty());
    for chunk in [&c0, &c1, &c2] {
        assert_eq!(walk_lines_header_count(chunk), count);
    }
    let chunks = [&c0, &c1, &c2];
    for i in 0..300usize {
        let lines = chunks[i / WALK_LINES_MAX_FRAMES];
        let at = WALK_LINES_HEADER + i % WALK_LINES_MAX_FRAMES;
        // Cast: a test index below 300.
        assert_eq!(walk_line_decode(lines[at]), (i as i32, false), "{i}");
    }
    // Chunk 0 is exactly the single array of the walk.
    let single = walk_lines_for(&frames);
    assert_eq!(single[WALK_LINES_HEADER..], c0[WALK_LINES_HEADER..]);
    // A walk of at most 128 frames has one chunk.
    assert!(walk_lines_chunk_for(&frames[..128], 1, count).is_empty());
    assert_eq!(walk_lines_header_count(&[1]), 0);
}

/// WALK21-4: a frame's lines entry encodes its resolved line as the chunk
/// builder does, so the slot `settle_walk_line` writes is the one an eager
/// capture would have built; the deferred sentinels never decode to a line.
#[test]
fn r21_walk_deferred_line_slot_encodes_like_the_chunk() {
    let entry = |line: i32, file: bool| StackTraceEntry {
        class_name: std::sync::Arc::from("p/C"),
        method_name: std::sync::Arc::from("m"),
        method_descriptor: None,
        source_file: file.then(|| std::sync::Arc::from("C.java")),
        line_number: line,
        byte_code_index: 0,
        class_id: Some(cratonvm_types::ClassId::new(9)),
        method_index: None,
    };
    for (line, file) in [(1, true), (42, false), (-1, true), (LINE_NUMBER_NATIVE, true)] {
        let e = entry(line, file);
        let chunk = walk_lines_chunk_for(std::slice::from_ref(&e), 0, 0);
        assert_eq!(chunk[WALK_LINES_HEADER], walk_line_encode(&e));
        assert_eq!(walk_line_decode(walk_line_encode(&e)), (line, !file));
    }
    for sentinel in [
        cratonvm_native_api::LINE_NUMBER_DEFERRED,
        cratonvm_native_api::LINE_NUMBER_DEFERRED_AT_BCI_ZERO,
    ] {
        // Clamped to 0, i.e. decoded as -2: never a real line of a frame
        // that could be read before its build settles it.
        assert_eq!(walk_line_decode(walk_line_encode(&entry(sentinel, true))).0, -2);
    }
}

/// WALK21-5: the option ordinals are read by name, in the universe's own
/// order (JDK 22 inserted `DROP_METHOD_INFO`), and answer the bit vector;
/// an undeclared name or another constant leaves it to the name loop.
#[test]
fn r21_walk_option_ordinals_answer_the_bit_vector() {
    let names: Vec<Option<String>> = [
        "RETAIN_CLASS_REFERENCE",
        "SHOW_REFLECT_FRAMES",
        "SHOW_HIDDEN_FRAMES",
        "DROP_METHOD_INFO",
    ]
    .iter()
    .map(|n| Some(n.to_string()))
    .collect();
    let ordinals = option_ordinals_from_names(&names);
    assert_eq!(ordinals, [0, 1, 2, 3]);
    let elements: i64 = (1 << 0) | (1 << 3);
    assert_eq!(option_ordinal_answer(&ordinals, elements, "RETAIN_CLASS_REFERENCE"), Some(true));
    assert_eq!(option_ordinal_answer(&ordinals, elements, "SHOW_REFLECT_FRAMES"), Some(false));
    assert_eq!(option_ordinal_answer(&ordinals, elements, "DROP_METHOD_INFO"), Some(true));
    assert_eq!(option_ordinal_answer(&ordinals, elements, "OTHER"), None);
    // An older JDK's universe: no DROP_METHOD_INFO, another order.
    let old: Vec<Option<String>> = ["SHOW_HIDDEN_FRAMES", "RETAIN_CLASS_REFERENCE", "SHOW_REFLECT_FRAMES"]
        .iter()
        .map(|n| Some(n.to_string()))
        .collect();
    let old = option_ordinals_from_names(&old);
    assert_eq!(old, [1, 2, 0, OPTION_ORDINAL_ABSENT]);
    assert_eq!(option_ordinal_answer(&old, 1, "SHOW_HIDDEN_FRAMES"), Some(true));
    assert_eq!(option_ordinal_answer(&old, 1, "DROP_METHOD_INFO"), None);
}

/// WALK21-7: the per-frame split divides capture / screen by the frames
/// listed, the builds by the frames each built, and the rest of `apply`
/// by the frames consumed.
#[test]
fn r21_walk_frame_split_divides_by_the_right_frames() {
    let mut split = WalkFrameSplit::default();
    // 200 frames listed, 7 built by the first batch, 193 by later ones.
    split.add(200, 7, Some((19_300, 193)), [20_000, 4_000, 700, 59_300]);
    split.add(200, 7, Some((19_300, 193)), [20_000, 4_000, 700, 59_300]);
    let line = split.line(0x10, 200);
    assert!(line.contains("frames=200 calls=2 built/call=200"), "{line}");
    assert!(line.contains("capture/frame=100 ns"), "{line}");
    assert!(line.contains("screen/frame=20 ns"), "{line}");
    assert!(line.contains("first-build/frame=100 ns"), "{line}");
    assert!(line.contains("batch-build/frame=100 ns"), "{line}");
    assert!(line.contains("java+function/frame=200 ns"), "{line}");
    assert!(line.contains("per-walk=84000 ns"), "{line}");
    // A walk with no later batch divides nothing by zero.
    let mut short = WalkFrameSplit::default();
    short.add(3, 3, None, [1, 2, 3, 4]);
    assert!(short.line(0, 3).contains("batch-build/frame=0 ns"));
}

/// WALK21-10: lines moved into a walk's holder answer the same chunks
/// (chunk 0 from its slot, chunk k from the deep holder's element k-1),
/// and a held walk without deep chunks covers only chunk 0.
#[test]
fn r21_walk_held_lines_answer_the_same_chunks() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let object = ctx
        .ensure_class_initialized("java/lang/Object")
        .unwrap_or_else(|_| panic!("Object"));
    let first = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    let chunk1 = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    let deep = ctx.try_new_ref_array(object, 1).expect("deep holder");
    ctx.set_array_element(deep, 0, Value::Object(Some(chunk1)));
    let holder = ctx.try_new_ref_array(object, 3).expect("holder");
    let first_pin = ctx.pin_native_root(first);
    let deep_pin = ctx.pin_native_root(deep);
    let holder_pin = ctx.pin_native_root(holder);
    let lines = WalkLines {
        pin: first_pin,
        lines: first,
        deep: Some((deep_pin, deep)),
        held: None,
    };
    assert_eq!(lines.chunk_of(&mut ctx, 5), Some(first));
    assert_eq!(lines.chunk_of(&mut ctx, 130), Some(chunk1));
    let held = lines.rehome(&mut ctx, holder_pin, holder, 0);
    assert_eq!(held.held, Some(0));
    assert!(held.covers(&mut ctx, 127));
    assert!(held.covers(&mut ctx, 255));
    assert!(!held.covers(&mut ctx, 256));
    assert_eq!(held.chunk_of(&mut ctx, 0), Some(first));
    assert_eq!(held.chunk_of(&mut ctx, 200), Some(chunk1));
    let shallow = WalkLines {
        pin: first_pin,
        lines: first,
        deep: None,
        held: None,
    }
    .rehome(&mut ctx, holder_pin, holder, 0);
    assert!(shallow.covers(&mut ctx, 10));
    assert!(!shallow.covers(&mut ctx, 128));
}

/// WALK21-3 (`CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT`, default ON): a
/// `jdk.internal.reflect` frame with a class id is a reflection frame
/// exactly when its class is assignable to `MethodAccessor` /
/// `ConstructorAccessor` (JDK 25 `isReflectionFrame`), whatever its name
/// says; a frame with no id keeps the name rule.
#[test]
fn r21_walk_exact_reflection_screen_asks_the_accessor_interfaces() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let mut class = |name: &str| {
        ctx.ensure_class_initialized(name)
            .unwrap_or_else(|_| panic!("{name}"))
    };
    let method_accessor = class("jdk/internal/reflect/MethodAccessor");
    let ctor_accessor = class("jdk/internal/reflect/ConstructorAccessor");
    let accessor_impl = class("jdk/internal/reflect/MethodAccessorImpl");
    let direct = class("jdk/internal/reflect/DirectMethodHandleAccessor");
    let adapter = class("jdk/internal/reflect/CsMethodAccessorAdapter");
    let field = class("jdk/internal/reflect/MethodHandleObjectFieldAccessorImpl");
    let ctor = class("jdk/internal/reflect/DirectConstructorHandleAccessor");
    ctx.set_interfaces(accessor_impl, vec![method_accessor]);
    ctx.set_superclass(direct, accessor_impl);
    ctx.set_superclass(adapter, accessor_impl);
    ctx.set_interfaces(ctor, vec![ctor_accessor]);
    let entry = |name: &str, class_id: Option<cratonvm_types::ClassId>| StackTraceEntry {
        class_name: std::sync::Arc::from(name),
        method_name: std::sync::Arc::from("invoke"),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: 0,
        class_id,
        method_index: None,
    };
    let exact = |name: &str, class_id| is_reflection_frame_entry(&ctx, &entry(name, class_id));
    assert!(exact("java/lang/reflect/Method", None));
    assert!(exact("java/lang/reflect/Constructor", None));
    assert!(exact("jdk/internal/reflect/DirectMethodHandleAccessor", Some(direct)));
    assert!(exact("jdk/internal/reflect/DirectConstructorHandleAccessor", Some(ctor)));
    // The two the name rule answered unlike the JDK.
    assert!(exact("jdk/internal/reflect/CsMethodAccessorAdapter", Some(adapter)));
    assert!(!exact("jdk/internal/reflect/MethodHandleObjectFieldAccessorImpl", Some(field)));
    assert!(!is_reflection_frame_class("jdk/internal/reflect/CsMethodAccessorAdapter"));
    assert!(is_reflection_frame_class(
        "jdk/internal/reflect/MethodHandleObjectFieldAccessorImpl"
    ));
    // No class id: the name rule. Outside the package: never.
    assert!(exact("jdk/internal/reflect/NativeMethodAccessorImpl", None));
    assert!(!exact("p/DirectMethodHandleAccessor", Some(direct)));
    assert!(!exact("java/lang/reflect/Field", None));
}

/// No redefinition since the walk (by the count's residue) answers
/// `false` without asking the ring.
#[test]
fn a_walk_at_the_current_count_was_not_redefined_since() {
    let cid = cratonvm_types::ClassId::new(7);
    let ctx = crate::test_utils::MockNativeContext::new();
    assert!(!sfi_class_redefined_since(&ctx, cid, 5, 5));
    assert!(!sfi_class_redefined_since(&ctx, cid, 5, 133));
    // A residue above `now` with no full lap behind it cannot be.
    assert!(!sfi_class_redefined_since(&ctx, cid, 100, 3));
}

/// nio21 wave 6 L1: a context with no per-VM log (the trait default,
/// the mock) answers `None`, and the reader asks the process ring; an
/// empty span is answered with no visit either way.
#[test]
fn r21w6_redefinition_reader_falls_back_to_the_process_ring() {
    use cratonvm_native_api::NativeClassAccess;
    let ctx = crate::test_utils::MockNativeContext::new();
    let mut seen = 0;
    let mut count = |_c: cratonvm_types::ClassId| seen += 1;
    assert_eq!(ctx.redefined_classes_between(3, 3, &mut count), None);
    let now = cratonvm_classloading::class_redefinition_count();
    assert!(vm_redefined_classes_between(&ctx, now, now, |_| seen += 1));
    assert_eq!(seen, 0);
}

#[test]
fn register_lang_stackwalker_adds_natives() {
    let mut r = NativeMethodRegistry::new();
    register_lang_stackwalker(&mut r);
    assert!(r
        .find(
            "java/lang/StackStreamFactory$AbstractStackWalker",
            "callStackWalk",
            "(JIII[Ljava/lang/Object;[Ljava/lang/Class;)Ljava/lang/Object;"
        )
        .is_some());
    assert!(r
        .find(
            "java/lang/StackStreamFactory$AbstractStackWalker",
            "fetchStackFrames",
            "(JJII[Ljava/lang/Object;)I"
        )
        .is_some());
    assert!(r
        .find("java/lang/StackFrameInfo", "getByteCodeIndex", "()I")
        .is_some());
    assert!(r
        .find(
            "java/lang/StackFrameInfo",
            "getDeclaringClass",
            "()Ljava/lang/Class;"
        )
        .is_some());
}

/// gc-common w19-g: `fetchStackFrames` now pins its receiver BEFORE the
/// allocating frame loop (it pinned the stale post-loop address) and
/// still releases every pin it takes. The mock never moves an object, so
/// the relocation itself cannot be staged here; this holds the balance.
#[test]
fn w19g_fetch_stack_frames_releases_its_pins() {
    let mut ctx = crate::test_utils::mock_ctx();
    let this = ctx.fresh_object_ref();
    let frames = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
    let before = ctx.native_pin_count_for_test();
    // JDK 25 shape: (this, int mode, long anchor, int lastBatchFrameCount,
    // int batchSize, int startIndex, T[] frames).
    let args = [
        Value::Object(Some(this)),
        Value::Int(0),
        Value::Long(0),
        Value::Int(0),
        Value::Int(4),
        Value::Int(0),
        Value::Object(Some(frames)),
    ];
    // No walk is active on this thread and the mock captures no frames,
    // so nothing is written.
    let r = native_fetch_stack_frames(&mut ctx, &args);
    assert!(matches!(r, Ok(Some(Value::Int(0)))), "got {r:?}");
    assert_eq!(ctx.native_pin_count_for_test(), before);
}

fn named(method: &str) -> cratonvm_native_api::StackTraceEntry {
    cratonvm_native_api::StackTraceEntry {
        class_name: std::sync::Arc::from("p/C"),
        method_name: std::sync::Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: -1,
        class_id: None,
        method_index: None,
    }
}

fn names(frames: &[cratonvm_native_api::StackTraceEntry]) -> Vec<String> {
    frames.iter().map(|f| f.method_name.to_string()).collect()
}

/// Interpreter round i1 wave 48, lane L3: the hidden stand-in frames go
/// before the frame each insertion names (its caller, the walk being
/// innermost first), in order, and one past the end appends.
#[test]
fn hidden_standin_frames_are_spliced_before_their_callers() {
    let frames = vec![named("target"), named("caller"), named("main")];
    let out = splice_hidden_standin_frames(
        frames,
        vec![
            (1, vec![named("holder"), named("invoker")]),
            (3, vec![named("tail")]),
        ],
    );
    assert_eq!(
        names(&out),
        ["target", "holder", "invoker", "caller", "main", "tail"]
    );
    let unchanged = splice_hidden_standin_frames(vec![named("a"), named("b")], Vec::new());
    assert_eq!(names(&unchanged), ["a", "b"]);
}

fn walk_entry(
    class: &str,
    method: &str,
    descriptor: &str,
    class_id: Option<u32>,
) -> cratonvm_native_api::StackTraceEntry {
    cratonvm_native_api::StackTraceEntry {
        class_name: std::sync::Arc::from(class),
        method_name: std::sync::Arc::from(method),
        method_descriptor: Some(std::sync::Arc::from(descriptor)),
        source_file: None,
        line_number: -1,
        byte_code_index: -1,
        class_id: class_id.map(cratonvm_types::ClassId::new),
        method_index: None,
    }
}

/// Interpreter round i6 wave 115, lane L3 (`CRATONVM_STANDIN_SPUN_IWA_FRAMES`):
/// a served `invokeWithArguments` target is found only under its direct
/// handle's holder, the call site's invoker and the array overload's
/// frame; its spun frames go before the invoker, and `n` is the holder's
/// parameters but the handle.
#[test]
fn i115_l3_spun_iwa_sites_need_the_whole_served_chain() {
    const OBJ: &str = "Ljava/lang/Object;";
    let frames = vec![
        walk_entry("p/C", "tInt2", "(II)I", Some(7)),
        walk_entry(
            "java/lang/invoke/DirectMethodHandle$Holder",
            "invokeStatic",
            &format!("({OBJ}II)I"),
            Some(1),
        ),
        walk_entry(
            "java/lang/invoke/Invokers$Holder",
            "invokeExact_MT",
            &format!("({OBJ}{OBJ}{OBJ}{OBJ}){OBJ}"),
            Some(2),
        ),
        walk_entry(
            "java/lang/invoke/MethodHandle",
            "invokeWithArguments",
            &format!("([{OBJ}){OBJ}"),
            Some(3),
        ),
        walk_entry("p/C", "main", "([Ljava/lang/String;)V", Some(7)),
    ];
    let sites = spun_iwa_sites(&frames);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].at, 2);
    assert_eq!(sites[0].arity, 2);
    assert_eq!(&*sites[0].holder, "invokeStatic");
    assert_eq!(&*sites[0].name, "tInt2");
    // Not under invokeWithArguments (a plain invokeExact): nothing.
    let mut exact = frames.clone();
    exact.remove(3);
    assert!(spun_iwa_sites(&exact).is_empty());
    assert!(!sites[0].holderless);
    // No holder named (a shape the image does not pregenerate): since
    // wave 138 a holderless site, before the invoker, which only the
    // record path answers.
    let mut unnamed = frames.clone();
    unnamed.remove(1);
    let holderless = spun_iwa_sites(&unnamed);
    assert_eq!(holderless.len(), 1);
    assert!(holderless[0].holderless);
    assert_eq!(holderless[0].at, 1);
    assert_eq!(&*holderless[0].name, "tInt2");
    // ... and none under a non-array overload.
    let mut list = unnamed.clone();
    list[2].method_descriptor = Some(std::sync::Arc::from("(Ljava/util/List;)Ljava/lang/Object;"));
    assert!(spun_iwa_sites(&list).is_empty());
    // A target with no class id: nothing.
    let mut anonymous = frames;
    anonymous[0].class_id = None;
    assert!(spun_iwa_sites(&anonymous).is_empty());
}

/// Interpreter round i7 wave 137, lane L3: the walk's served
/// `invokeWithArguments` frames (innermost first) pair with the served
/// rows (outermost first) only when the counts agree; the `List`
/// overload's own frame is not counted.
#[test]
fn i137_l3_iwa_frame_rows_pair_innermost_frame_with_innermost_row() {
    const ARRAY: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
    const LIST: &str = "(Ljava/util/List;)Ljava/lang/Object;";
    let mh = "java/lang/invoke/MethodHandle";
    let frames = vec![
        walk_entry("p/C", "inner", "()V", Some(7)),
        walk_entry(mh, "invokeWithArguments", ARRAY, Some(3)),
        walk_entry("p/C", "middle", "()V", Some(7)),
        walk_entry(mh, "invokeWithArguments", ARRAY, Some(3)),
        walk_entry(mh, "invokeWithArguments", LIST, Some(3)),
        walk_entry("p/C", "main", "([Ljava/lang/String;)V", Some(7)),
    ];
    assert_eq!(iwa_frame_rows(&frames, 2), Some(vec![(1, 1), (3, 0)]));
    assert_eq!(iwa_frame_rows(&frames, 1), None, "a row listed by no frame");
    assert_eq!(iwa_frame_rows(&frames, 3), None);
    assert_eq!(iwa_frame_rows(&frames[..1], 0), Some(Vec::new()));
    let mut dotted = frames;
    dotted[1].class_name = std::sync::Arc::from("java.lang.invoke.MethodHandle");
    assert_eq!(iwa_frame_rows(&dotted, 2), Some(vec![(1, 1), (3, 0)]));
}

#[test]
fn i115_l3_descriptor_parameter_count_reads_every_kind() {
    assert_eq!(descriptor_parameter_count("()V"), Some(0));
    assert_eq!(
        descriptor_parameter_count("(Ljava/lang/Object;I[[JLp/C;D)V"),
        Some(5)
    );
    assert_eq!(descriptor_parameter_count("(Q)V"), None);
    assert_eq!(descriptor_parameter_count("V"), None);
    assert_eq!(
        generic_form_descriptor(2),
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
    );
}

/// Interpreter round i6 wave 121, lane L3: an `invokeExact` site's type
/// as `MethodType.basicType()` spells it, and the frames the linker step
/// may stand on.
#[test]
fn i121_l3_spun_linker_basic_type_and_callee() {
    const OBJ: &str = "Ljava/lang/Object;";
    assert_eq!(basic_call_descriptor("(I)I").as_deref(), Some("(I)I"));
    assert_eq!(
        basic_call_descriptor("(Ljava/lang/String;D)D").as_deref(),
        Some(format!("({OBJ}D)D").as_str())
    );
    assert_eq!(
        basic_call_descriptor("(ZBSC[I[[Lp/C;J)Z").as_deref(),
        Some(format!("(IIII{OBJ}{OBJ}J)I").as_str())
    );
    assert_eq!(basic_call_descriptor("()V").as_deref(), Some("()V"));
    assert_eq!(basic_call_descriptor("(V)V"), None);
    assert_eq!(basic_call_descriptor("(Q)V"), None);
    // 127 longs take 254 slots: the customized linker, not listed.
    assert_eq!(basic_call_descriptor(&format!("({})V", "J".repeat(127))), None);
    assert!(basic_call_descriptor(&format!("({})V", "J".repeat(126))).is_some());
    let at = |class: &str, method: &str| walk_entry(class, method, "()V", None);
    assert!(spun_linker_callee(&at(
        "java/lang/invoke/DirectMethodHandle$Holder",
        "invokeStatic"
    )));
    assert!(spun_linker_callee(&at("java/lang/invoke/LambdaForm$MH/0x1", "invoke")));
    assert!(spun_linker_callee(&at("java.lang.invoke.LambdaForm$DMH/0x2", "invokeStatic")));
    assert!(!spun_linker_callee(&at("java/lang/invoke/Invokers$Holder", "invokeExact_MT")));
    assert!(!spun_linker_callee(&at("java/lang/invoke/LambdaForm$MH/0x3", "invokeExact_MT")));
    assert!(!spun_linker_callee(&at("p/C", "target")));
}

/// A VM that lists no hidden stand-in frames (the mock's default) never
/// asks the walker's options.
#[test]
fn a_vm_that_lists_no_hidden_standins_does_not_ask_the_walker() {
    let mut ctx = crate::test_utils::mock_ctx();
    let read = std::cell::Cell::new(false);
    let (frames, asked) =
        add_hidden_standin_frames(&mut ctx, vec![named("target"), named("caller")], |_| {
            read.set(true);
            true
        });
    assert!(!read.get(), "the walker's options were read");
    assert_eq!(asked, None);
    assert_eq!(names(&frames), ["target", "caller"]);
}
