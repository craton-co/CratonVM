use super::*;
use crate::classloading::ClassId;
use crate::threading::jvm_thread::ThreadId;
use crate::types::Value;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

/// A version-52 class `obsolete/VmProbe` whose one method
/// `static value()I` is `ldc #8; ireturn`, `#8` being
/// `CONSTANT_Integer constant`.
fn ldc_class(constant: i32) -> Vec<u8> {
    ldc_class_with_code(constant, &[0x12, 8, 0xac]) // ldc #8; ireturn
}

/// [`ldc_class`] whose `value()I` is `code` (max_stack 1, no locals).
fn ldc_class_with_code(constant: i32, code: &[u8]) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, "obsolete/VmProbe"); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(3); // #8 Integer
    b.extend_from_slice(&constant.to_be_bytes());
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

/// A frame of `value()` as a call would have pushed it, from the body
/// the class has now.
fn value_frame(shared: &SharedVm, cid: ClassId) -> Option<Frame> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(cid)?;
    let method = class.methods.iter().find(|m| &*m.name == "value")?;
    let code = method.code()?;
    Some(Frame::new_from_arcs(
        cid,
        Arc::clone(&class.name),
        Arc::from("value"),
        Arc::from("()I"),
        None,
        crate::runtime::frame::padded_bytecode(&code.code[..]),
        Arc::from(Vec::<ExceptionTableEntry>::new()),
        code.max_stack,
        code.max_locals,
        &[],
    ))
}

/// The i18-L3 page's unit test: a frame running `value()` when its class
/// is redefined with a pool that puts another constant at the index its
/// `ldc` names still loads the OLD constant -- as HotSpot runs an
/// obsolete method -- while a fresh call runs the new body. Before wave
/// 19 the old frame read the new pool and returned 80000.
#[test]
fn a_frame_running_across_a_redefinition_keeps_its_constants() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_119), "obsolete-frames");
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "obsolete/VmProbe",
            &ldc_class(70_000),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            // The stripped test VM could not define it: nothing to run.
            Err(_) => return,
        }
    };
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    let stamp = thread.frames[0].redefine_stamp();
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    assert!(
        frame_runs_replaced_code(&shared, &thread.frames[0]),
        "built before the redefinition, not yet moved"
    );

    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    let moved = &thread.frames[0];
    assert!(moved.redefine_stamp() > stamp);
    assert!(
        moved.runs_obsolete_method(),
        "its constant differs from the current body's: an obsolete method"
    );
    assert!(frame_runs_replaced_code(&shared, moved));
    assert_eq!(retired(moved), 1, "the code it ran is kept with it");
    let operand = moved.code[1];
    assert_ne!(
        operand, 8,
        "the ldc operand now names the old constant's merged index"
    );
    {
        let cm = shared.classes.class_manager.read();
        let pool = &cm.get_class(cid).expect("loaded").constant_pool;
        assert!(matches!(
            pool.get(u16::from(operand)),
            Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                70_000
            ))
        ));
    }
    // Idempotent: nothing moved since.
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    assert_eq!(retired(&thread.frames[0]), 1);

    // Run the old frame to its `ireturn`: the old constant.
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the frame that was running keeps its constant, got {old:?}"
    );
    // A fresh call runs the new body.
    let fresh =
        crate::runtime::interpreter::execute(&shared, &mut thread, cid, "value", "()I", &[]);
    assert!(
        matches!(fresh, Ok(Some(Value::Int(80_000)))),
        "a new call runs the new body, got {fresh:?}"
    );
}

/// A frame built after the redefinition runs the current body and is
/// left alone; a frame whose body equals the new one modulo the pool
/// (EMCP) is moved but not obsolete. Here every constant kept its index,
/// so the older frame keeps its code and is only restamped (wave 21,
/// lane L4): nothing is retired.
#[test]
fn current_and_emcp_frames_are_not_obsolete() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_120), "obsolete-frames-emcp");
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "obsolete/VmProbe",
            &ldc_class(5_000_000),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            Err(_) => return,
        }
    };
    let Some(before) = value_frame(&shared, cid) else {
        return;
    };
    let stamp = before.redefine_stamp();
    let code = Arc::clone(&before.code);
    thread.frames.push(before);
    // The same bytes again: every constant keeps its index.
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(5_000_000), RedefineOptions::default())
        .expect("an identical redefinition succeeds");
    let Some(after) = value_frame(&shared, cid) else {
        return;
    };
    assert!(!frame_runs_replaced_code(&shared, &after), "built after");
    thread.frames.push(after);
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    assert!(
        !thread.frames[0].runs_obsolete_method(),
        "EMCP: not obsolete"
    );
    assert!(!frame_runs_replaced_code(&shared, &thread.frames[0]));
    assert!(thread.frames[0].redefine_stamp() > stamp, "restamped");
    assert!(
        Arc::ptr_eq(&thread.frames[0].code, &code),
        "an unchanged body keeps its code"
    );
    assert_eq!(
        retired(&thread.frames[0]),
        0,
        "no private copy, nothing retired"
    );
}

/// The code allocations `frame` keeps from its earlier moves.
fn retired(frame: &Frame) -> usize {
    frame
        .replaced_body()
        .map_or(0, |replaced| replaced.retired_code.len())
}

/// The i21-L4 page's unit test (interpreter round i1 wave 22, lane L3):
/// one frame across 101 redefinitions that alternate its constant between
/// two values at the same index, moved after each. It used to retire one
/// private copy per move onto the thread (100 by the end); it now
/// alternates between two allocations -- the one it was built on and one
/// private copy -- and keeps the other with itself. It still returns its
/// own constant, and a fresh call the class's.
#[test]
fn a_frame_toggled_between_two_versions_keeps_two_code_copies() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_123), "obsolete-frames-toggle");
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    let mut seen: Vec<usize> = Vec::new();
    for round in 1..=101 {
        let constant = if round % 2 == 1 { 80_000 } else { 70_000 };
        redefine_probe(&shared, cid, ldc_class(constant));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        let moved = &thread.frames[0];
        assert!(
            retired(moved) <= 1,
            "round {round}: {} copies kept",
            retired(moved)
        );
        // Cast: an allocation address, compared only.
        let at = moved.code.as_ptr() as usize;
        if !seen.contains(&at) {
            seen.push(at);
        }
        assert_eq!(
            moved.runs_obsolete_method(),
            round % 2 == 1,
            "round {round}: obsolete only while the class holds the other constant"
        );
    }
    assert_eq!(seen.len(), 2, "two allocations, reused in turn");

    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the frame that was running keeps its constant, got {old:?}"
    );
    let fresh =
        crate::runtime::interpreter::execute(&shared, &mut thread, cid, "value", "()I", &[]);
    assert!(
        matches!(fresh, Ok(Some(Value::Int(80_000)))),
        "a new call runs the new body, got {fresh:?}"
    );
}

/// A version-52 `obsolete/VmProbe` like [`ldc_class`] (`value()I` is
/// `ldc #8; ireturn`, `#8` = `CONSTANT_Integer constant`), whose method
/// has a `LineNumberTable` putting pc 0 on `line`, and whose pool ends
/// with `padding` unused UTF-8 entries.
fn probe_class(constant: i32, line: u16, padding: u16) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&(10 + padding).to_be_bytes()); // constant_pool_count
    utf8(&mut b, "obsolete/VmProbe"); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(3); // #8 Integer
    b.extend_from_slice(&constant.to_be_bytes());
    utf8(&mut b, "LineNumberTable"); // #9
    for i in 0..padding {
        utf8(&mut b, &("pad".to_string() + &i.to_string())); // #10..
    }
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
    let code = [0x12, 8, 0xac]; // ldc #8; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32 + 12).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0]); // no handlers
    b.extend_from_slice(&[0, 1]); // one attribute
    b.extend_from_slice(&[0, 9, 0, 0, 0, 6, 0, 1, 0, 0]); // LineNumberTable, pc 0
    b.extend_from_slice(&line.to_be_bytes());
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

fn define_probe(shared: &SharedVm, bytes: &[u8]) -> Option<ClassId> {
    shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            "obsolete/VmProbe",
            bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .ok()
}

/// Redefine the probe class the way the VM's path leaves THIS VM: the
/// swap, then what `vm_init::resolution_invalidate_adapter` does for the
/// VM that owns the class store -- a new resolution epoch and a sweep of
/// the class's recorded constants. A bare `SharedVm` is not in that
/// adapter's live-VM registry (`Vm::new` registers), so without the last
/// two lines a constant one frame recorded at an index before the swap is
/// served to another frame's `ldc` of that index after it (interpreter
/// round i1 wave 25, lane L3: the thaw half of
/// `a_conversion_the_busy_class_manager_deferred_runs_before_the_next_bytecode`
/// read `#9`'s old 70000 from the resolution cache, not from the pool).
fn redefine_probe(shared: &SharedVm, cid: ClassId, bytes: Vec<u8>) {
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, bytes, RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    crate::runtime::interpreter::bump_resolution_epoch_in(shared);
    shared
        .classes
        .resolution_cache
        .write()
        .invalidate_class(cid);
}

/// The i19-L3 page's case 1 (interpreter round i1 wave 21, lane L4): the
/// new pool has 309 entries, so the old `ldc #8`'s constant is appended
/// above index 255 where one operand byte cannot name it. The frame used
/// to be left untranslated and read the NEW constant at `#8`; it now
/// runs on a stream that reads the merged index, off the fast path, and
/// returns the old constant as HotSpot's obsolete method does.
#[test]
fn an_ldc_whose_old_constant_lands_above_a_byte_runs_on_a_widened_stream() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_121), "obsolete-frames-wide");
    let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
        return;
    };
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    redefine_probe(&shared, cid, probe_class(80_000, 10, 299));
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    let moved = &thread.frames[0];
    assert!(moved.runs_obsolete_method(), "a widened body is obsolete");
    assert_eq!(moved.code[1], 8, "the operand byte is left as it was");
    let Some(stream) = moved.widened_stream() else {
        unreachable!("the frame runs on a widened stream");
    };
    let Some((cratonvm_reader::instruction::Instruction::LdcW(merged), 2)) = stream.resolve(0)
    else {
        unreachable!("pc 0 decodes as ldc_w");
    };
    let merged = *merged;
    assert!(merged > 255, "appended after the 309-entry pool: #{merged}");
    {
        let cm = shared.classes.class_manager.read();
        let pool = &cm.get_class(cid).expect("loaded").constant_pool;
        assert!(matches!(
            pool.get(merged),
            Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                70_000
            ))
        ));
    }
    assert!(
        !super::super::frame_fast_path_admitted(&shared, &thread.frames[0]),
        "the fast path would read the stale operand byte"
    );
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the frame that was running keeps its constant, got {old:?}"
    );
    let fresh =
        crate::runtime::interpreter::execute(&shared, &mut thread, cid, "value", "()I", &[]);
    assert!(
        matches!(fresh, Ok(Some(Value::Int(80_000)))),
        "a new call runs the new body, got {fresh:?}"
    );
}

/// The i19-L3 line-number page (interpreter round i1 wave 21, lane L4):
/// a frame running a body whose `LineNumberTable` a redefinition replaced
/// reports the old body's line in a stack trace, through a second
/// redefinition too; a frame of the current body resolves from the class.
#[test]
fn a_moved_frame_reports_the_lines_of_its_own_body() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_122), "obsolete-frames-lines");
    let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
        return;
    };
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    // Same code and pool, the statement moved to line 20: EMCP.
    redefine_probe(&shared, cid, probe_class(70_000, 20, 0));
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    assert!(!thread.frames[0].runs_obsolete_method(), "EMCP");
    assert_eq!(thread.frames[0].own_line_number(0), Some(10));
    let trace = crate::runtime::stackwalker::capture_frames_no_lines_exact(&thread.frames);
    assert_eq!(trace[0].line_number, 10, "the line of the source it runs");

    // Moved again, it keeps its own table rather than the one it was
    // stamped against (line 20's).
    redefine_probe(&shared, cid, probe_class(70_000, 30, 0));
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    assert_eq!(thread.frames[0].own_line_number(0), Some(10));

    let Some(current) = value_frame(&shared, cid) else {
        return;
    };
    assert_eq!(current.own_line_number(0), None, "the class answers");
    thread.frames.push(current);
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    let trace = crate::runtime::stackwalker::capture_frames_no_lines_exact(&thread.frames);
    assert_eq!(trace[0].line_number, 10);
    assert_eq!(
        trace[1].line_number,
        crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN,
        "resolved later, from the class"
    );
    // Wave 22, lane L3: the EMCP frame with its own lines stays off OSR
    // (an OSR body would report the current body's lines); the current
    // frame does not.
    assert!(!thread.frames[0].runs_obsolete_method());
    assert!(frame_runs_replaced_code(&shared, &thread.frames[0]));
    assert!(!frame_runs_replaced_code(&shared, &thread.frames[1]));
}

/// Interpreter round i1 wave 39, lane L3: a `StackWalker` capture gives
/// an activation begun before its class's redefinition no line -- an
/// EMCP one with identical bytes and lines too, as HotSpot's walk does
/// (`L3W39HotSwapWalkerLines.java`) -- before and
/// after its thread moved it, while a frame of the current body keeps its
/// line and the `Throwable`-shaped capture keeps the old activation's.
#[test]
fn a_stack_walk_gives_an_old_activation_no_line() {
    use crate::runtime::stackwalker::{
        capture_full_trace, capture_stack_walk_trace, LINE_NUMBER_UNKNOWN,
    };
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_139), "obsolete-frames-walk");
    let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
        return;
    };
    let Some(old) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(old);
    redefine_probe(&shared, cid, probe_class(70_000, 10, 0));
    {
        // Not moved yet: its stale stamp says it.
        let cm = shared.classes.class_manager.read();
        let walk = capture_stack_walk_trace(&cm, &thread.frames);
        assert_eq!(walk[0].line_number, LINE_NUMBER_UNKNOWN, "unmoved");
    }
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    assert!(!thread.frames[0].runs_obsolete_method(), "identical: EMCP");
    assert!(thread.frames[0].predates_its_class_redefinition());
    let Some(current) = value_frame(&shared, cid) else {
        return;
    };
    assert!(!current.predates_its_class_redefinition());
    thread.frames.push(current);
    let cm = shared.classes.class_manager.read();
    let walk = capture_stack_walk_trace(&cm, &thread.frames);
    assert_eq!(
        walk[0].line_number, LINE_NUMBER_UNKNOWN,
        "the old activation"
    );
    assert_eq!(walk[1].line_number, 10, "the current body's line");
    let full = capture_full_trace(&cm.class_store, &thread.frames);
    assert_eq!(full[0].line_number, 10, "a Throwable keeps the old line");
    assert_eq!(full[1].line_number, 10);
}

/// Interpreter round i1 wave 22, lane L3: a frame frozen into a
/// continuation before its class was redefined, thawed onto a carrier
/// that had already looked at that redefinition, is moved before it runs
/// (`convert_thawed_frames`, from `resume_continuation`) and loads its
/// own constant. Before, the carrier's `redefinitions_seen` kept every
/// conversion pass away until the next redefinition anywhere, and the
/// frame read the new pool: 80000.
#[test]
fn a_frame_thawed_after_a_redefinition_is_moved_before_it_runs() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_125), "obsolete-frames-thaw");
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    let frozen = frame.to_frozen_frame();
    redefine_probe(&shared, cid, ldc_class(80_000));
    // The carrier looks at the redefinition with no frame of the class.
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    thread.frames.push(Frame::from_frozen_frame(frozen));
    convert_thawed_frames(&shared, &mut thread);
    assert!(
        thread.frames[0].runs_obsolete_method(),
        "moved onto its translated body before it runs"
    );
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the thawed frame keeps its constant, got {old:?}"
    );
}

/// Interpreter round i1 wave 28, lane L3: a deoptimization sink rebuilds
/// the frame of a compiled activation from the code the body was compiled
/// from, after the class was redefined under the running activation.
/// `Frame::new_pooled` stamps that frame current, so no conversion pass
/// ever moved it and its `ldc #8` read the new pool: 80000.
/// `stamp_frame_rebuilt_from_compiled_code` restamps it with the body's
/// compile stamp and moves it before it runs.
#[test]
fn a_frame_rebuilt_from_a_stale_compiled_body_reads_its_own_constants() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_133), "obsolete-frames-deopt");
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    // The body is compiled now, from the class's current code.
    let compile_stamp = cratonvm_classloading::class_redefinition_count();
    let Some(compiled_from) = value_frame(&shared, cid) else {
        return;
    };
    let old_code = Arc::clone(&compiled_from.code);
    redefine_probe(&shared, cid, ldc_class(80_000));
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    // The trap: the sink rebuilds the frame from the old code, stamped now.
    let rebuilt = || {
        Frame::new_from_arcs(
            cid,
            Arc::from("obsolete/VmProbe"),
            Arc::from("value"),
            Arc::from("()I"),
            None,
            Arc::clone(&old_code),
            Arc::from(Vec::<ExceptionTableEntry>::new()),
            1,
            0,
            &[],
        )
    };
    // The control: stamped current, the frame is never moved.
    thread.frames.push(rebuilt());
    convert_thawed_frames(&shared, &mut thread);
    assert!(
        !thread.frames[0].runs_obsolete_method(),
        "a frame stamped current is not taken for a replaced body"
    );
    let unstamped = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(unstamped, Ok(Some(Value::Int(80_000)))),
        "the control reads the new pool at its old index, got {unstamped:?}"
    );

    // `execute_frame` leaves the finished frame on the stack: start each
    // case from an empty one, so `frames[0]` is the frame under test.
    thread.frames.clear();
    thread.frames.push(rebuilt());
    stamp_frame_rebuilt_from_compiled_code(&shared, &mut thread, Some(compile_stamp));
    assert!(
        thread.frames[0].runs_obsolete_method(),
        "moved onto its translated body before it runs"
    );
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the rebuilt frame keeps its constant, got {old:?}"
    );

    // A body compiled after the redefinition: its stamp changes nothing.
    let now = cratonvm_classloading::class_redefinition_count();
    let Some(current) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.clear();
    thread.frames.push(current);
    stamp_frame_rebuilt_from_compiled_code(&shared, &mut thread, Some(now));
    assert!(!thread.frames[0].runs_obsolete_method());
    let fresh = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(fresh, Ok(Some(Value::Int(80_000)))),
        "the current body reads the current pool, got {fresh:?}"
    );
}

/// Interpreter round i1 wave 25, lane L3: a continuation remounted after
/// the redefinition of a class none of its frames runs takes no
/// conversion pass -- it used to take the class-manager lock and look up
/// every frame's class on every remount once any class of the process
/// had been redefined -- when the redefinition ring can say so.
#[test]
fn a_thaw_after_an_unrelated_redefinition_takes_no_conversion_pass() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let unrelated = Frame::new(
        ClassId::new(0x00F1_0C78),
        "obsolete/Unrelated".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        Vec::new(),
        1,
        1,
        &[],
    );
    let stamp = unrelated.redefine_stamp();
    let mut thread = JvmThread::new(ThreadId(4_153), "obsolete-frames-unrelated-thaw");
    thread.frames.push(unrelated);
    redefine_probe(&shared, cid, ldc_class(80_000));
    let seen = thread.redefinitions_seen;
    let before = cratonvm_classloading::class_redefinition_count();
    let answerable =
        cratonvm_classloading::for_each_redefined_class_between(stamp, before, |_| {});
    convert_thawed_frames(&shared, &mut thread);
    // Other tests redefine classes concurrently: only a quiet span, which
    // the ring answered, says what the pass would have done.
    if answerable && cratonvm_classloading::class_redefinition_count() == before {
        assert_eq!(
            thread.redefinitions_seen, seen,
            "no pass for frames no recorded redefinition names"
        );
    }
}

/// Interpreter round i1 wave 22, lane L3: a Throwable's backtrace and a
/// blocked thread's published snapshot, both captured in a body whose
/// line table a redefinition then replaces, report the line of the body
/// they were captured in -- the backtrace through its capture count
/// (`resolve_lines_as_captured`), the snapshot because
/// `resolve_captured_lines_of_class` ran before the swap (as
/// `redefine_class_with` does). Both used to be resolved when read, from
/// the new table (line 20 here); HotSpot keeps the class version the
/// element was captured in.
#[test]
fn traces_captured_before_a_redefinition_keep_their_lines() {
    use crate::runtime::stackwalker::{capture_frames_no_lines, capture_frames_no_lines_exact};
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
        return;
    };
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    let frames = [frame];

    let throwable = shared.mem.heap.alloc_object(ClassId::new(7), 1);
    shared.store_throwable_stack_trace(
        throwable,
        Arc::from(capture_frames_no_lines_exact(&frames)),
    );
    let registry = &shared.threads.thread_registry;
    let tid = ThreadId(4_124);
    registry.register(tid, "obsolete-frames-blocked", None);
    registry.set_frame_trace(
        tid,
        Arc::new(parking_lot::Mutex::new(capture_frames_no_lines(&frames))),
    );

    {
        let cm = shared.classes.class_manager.read();
        resolve_captured_lines_of_class(&shared, &cm.class_store, cid);
    }
    // Same code and pool; the statement moved to line 20.
    redefine_probe(&shared, cid, probe_class(70_000, 20, 0));

    let thrown = shared
        .throwable_stack_trace_for(throwable)
        .expect("the backtrace is stored");
    assert_eq!(thrown[0].line_number, 10, "the Throwable's own body");
    let blocked = {
        let cm = shared.classes.class_manager.read();
        registry.frame_trace_of_resolved(tid, &cm.class_store)
    };
    assert_eq!(blocked[0].line_number, 10, "the blocked thread's own body");

    // A trace captured after the swap resolves from the new table.
    let Some(fresh) = value_frame(&shared, cid) else {
        return;
    };
    let later = shared.mem.heap.alloc_object(ClassId::new(7), 1);
    shared
        .store_throwable_stack_trace(later, Arc::from(capture_frames_no_lines_exact(&[fresh])));
    let thrown = shared
        .throwable_stack_trace_for(later)
        .expect("the backtrace is stored");
    assert_eq!(thrown[0].line_number, 20, "the current body");
}

/// Interpreter round i1 wave 38, lane L3
/// (`L3/L3W38HotSwapCompiledLines`, row `entry`): a COMPILED activation of
/// a body compiled before its class's redefinition keeps running it, and
/// a Throwable it captures AFTER the redefinition reports the line of that
/// body -- through the frame's compile stamp -- not the new body's table
/// read at the old bci. A compiled frame with no stamp, and one compiled
/// after the redefinition, resolve from the class as it is.
#[test]
fn a_compiled_frame_captured_after_a_redefinition_reports_its_own_body() {
    use crate::runtime::stackwalker::BacktraceFrame;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &probe_class(70_000, 10, 0)) else {
        return;
    };
    let compiled_before = cratonvm_classloading::class_redefinition_count();
    // Same code and pool; the statement moved to line 20.
    redefine_probe(&shared, cid, probe_class(70_000, 20, 0));
    let compiled_after = cratonvm_classloading::class_redefinition_count();
    let stored = |cp_stamp: Option<u64>| {
        let throwable = shared.mem.heap.alloc_object(ClassId::new(7), 1);
        let frames = vec![BacktraceFrame::Compiled {
            label: Arc::from("obsolete/VmProbe.value:()I"),
            owner_class_id: cid.as_u32(),
            bci: 0,
            cp_stamp,
        }];
        // Captured now, after the redefinition.
        shared.store_throwable_backtrace(throwable, frames.into_boxed_slice());
        let whole = shared
            .throwable_stack_trace_for(throwable)
            .expect("the backtrace is stored");
        let one = shared
            .throwable_stack_trace_entry_for(throwable, 0)
            .expect("the entry is stored");
        assert_eq!(whole[0].line_number, one.line_number, "both readers agree");
        one.line_number
    };
    assert_eq!(
        stored(Some(compiled_before)),
        10,
        "the body it was compiled from"
    );
    assert_eq!(stored(None), 20, "no stamp: the class as it is");
    assert_eq!(
        stored(Some(compiled_after)),
        20,
        "compiled from the new body"
    );
}

/// Interpreter round i1 wave 23, lane L3 (the i19-L3 atomicity page's
/// "found but not pinned down" item, and its window 3): a frame stamped
/// while its class's redefinition was under way -- a lock-free door built
/// it between the swap's two advances -- is translated only when its code
/// is not the class's current body; and a frame of the OLD body stamped
/// after the swap (pushed from an invoke-cache entry validated before it)
/// is translated when its thread next looks. Here the new body (`nop; ldc
/// #8; ireturn`) differs from the old (`ldc #8; ireturn`): a frame of the
/// NEW body with an in-swap stamp used to be translated through the old
/// pool's map, its `ldc #8` read as the OLD `#8`, and it returned 70000;
/// the old-body frame stamped after the swap was left alone and returned
/// 80000.
#[test]
fn a_frame_built_during_the_swap_is_translated_only_from_the_old_body() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let (Some(mut old_frame), Some(mut late_frame)) =
        (value_frame(&shared, cid), value_frame(&shared, cid))
    else {
        return;
    };
    // Created before the redefinition: each looks at it when converting.
    let mut current = JvmThread::new(ThreadId(4_127), "obsolete-frames-window-new");
    let mut stale = JvmThread::new(ThreadId(4_128), "obsolete-frames-window-old");
    let mut late = JvmThread::new(ThreadId(4_131), "obsolete-frames-window-late");
    redefine_probe(
        &shared,
        cid,
        ldc_class_with_code(80_000, &[0x00, 0x12, 8, 0xac]),
    );
    let Some(mut new_frame) = value_frame(&shared, cid) else {
        return;
    };
    let (began, latest) = {
        let cm = shared.classes.class_manager.read();
        let Some(history) = cm.redefinition_history(cid) else {
            unreachable!("the class was redefined");
        };
        (
            history.latest_redefinition_began(),
            history.latest_redefinition(),
        )
    };
    assert!(began < latest, "the swap spans two advances");
    // As a door that held no class-manager lock would have stamped them.
    old_frame.restamp_redefined_body(began, false, None);
    new_frame.restamp_redefined_body(began, false, None);
    let new_code = Arc::clone(&new_frame.code);

    current.frames.push(new_frame);
    convert_obsolete_frames_if_redefined(&shared, &mut current);
    assert!(!current.frames[0].runs_obsolete_method());
    assert!(
        Arc::ptr_eq(&current.frames[0].code, &new_code),
        "the current body keeps its code"
    );
    assert_eq!(current.frames[0].redefine_stamp(), latest, "restamped");
    let fresh = super::super::execute_frame(&shared, &mut current);
    assert!(
        matches!(fresh, Ok(Some(Value::Int(80_000)))),
        "a frame of the new body reads the new pool, got {fresh:?}"
    );

    stale.frames.push(old_frame);
    convert_obsolete_frames_if_redefined(&shared, &mut stale);
    assert!(stale.frames[0].runs_obsolete_method(), "translated");
    let old = super::super::execute_frame(&shared, &mut stale);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "a frame of the old body keeps its constant, got {old:?}"
    );

    // Window 3: the old body, stamped as if pushed after the swap.
    late_frame.restamp_redefined_body(latest, false, None);
    // Its stamp says current, but a back edge must not OSR it into a
    // body compiled from the new bytecode before its thread converts it
    // (wave 24); a frame of the new body may be.
    assert!(frame_runs_replaced_code(&shared, &late_frame), "window 3");
    if let Some(fresh_frame) = value_frame(&shared, cid) {
        assert!(
            !frame_runs_replaced_code(&shared, &fresh_frame),
            "the new body"
        );
    }
    late.frames.push(late_frame);
    convert_obsolete_frames_if_redefined(&shared, &mut late);
    assert!(late.frames[0].runs_obsolete_method(), "translated");
    let old = super::super::execute_frame(&shared, &mut late);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "a frame of the old body pushed after the swap keeps its constant, got {old:?}"
    );
}

/// Interpreter round i1 wave 23, lane L3 (the i21-L4 page): a frame
/// moved through many redefinitions that each give it DIFFERENT bytes
/// (every one appends the previous constant, so the old `ldc` moves one
/// index up each time) keeps one retired copy per move while its dispatch
/// loop has not reached its top on it, and frees them once it has
/// (`ReplacedBody::code_reached_top`, set where the loop refreshes its
/// fast-path gate). Each move that changes the code allocation also
/// bumps the stack's `code_moves`, which the loop's gate compares.
#[test]
fn a_frame_run_since_its_last_move_frees_the_copies_it_ran_before() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_129), "obsolete-frames-drain");
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    for round in 1..=50 {
        redefine_probe(&shared, cid, ldc_class(80_000 + round));
        let moves = thread.frames.code_moves();
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert_ne!(thread.frames.code_moves(), moves, "round {round}: moved");
        let moved = &thread.frames[0];
        assert!(
            retired(moved) <= 1,
            "round {round}: {} copies kept",
            retired(moved)
        );
        // The dispatch loop reaches its top on the moved frame.
        if let Some(replaced) = moved.replaced_body() {
            replaced.note_code_reached_top();
        }
    }
    // Two more moves with no loop top in between: the copies stay.
    redefine_probe(&shared, cid, ldc_class(90_001));
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    assert_eq!(retired(&thread.frames[0]), 1);
    redefine_probe(&shared, cid, ldc_class(90_002));
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    assert_eq!(
        retired(&thread.frames[0]),
        2,
        "a loop suspended since the last move may still hold the older copy"
    );
    // Wave 24: more moves with no loop top -- a frame suspended in one
    // instruction while nested code keeps moving it -- free each copy no
    // loop ever loaded instead of keeping one per move.
    for round in 3..=12 {
        redefine_probe(&shared, cid, ldc_class(90_000 + round));
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
        assert_eq!(
            retired(&thread.frames[0]),
            UNLOADED_COPIES_KEPT,
            "round {round}: the held copy and one more"
        );
    }

    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the frame keeps its constant, got {old:?}"
    );
}

/// Interpreter round i1 wave 23, lane L3 (the i22-L3 census proposal and
/// the i19-L3 untranslatable-frames page, case 2): a thread parked in a
/// method across 300 redefinitions that each renumber its class's pool
/// differently -- past both of the history's soft caps -- wakes with its
/// frame translatable, because the census reports its frame's stamp
/// (`prune_histories_by_census`) and the history keeps what it needs. Before, the
/// 257th step dropped the first and the frame read the new pool: 70300.
/// Once it has moved, the census lets the history go.
#[test]
fn a_thread_parked_across_hundreds_of_renumberings_stays_translatable() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let mut parked = JvmThread::new(ThreadId(4_130), "obsolete-frames-parked");
    let registry = &shared.threads.thread_registry;
    registry.register(parked.thread_id, "obsolete-frames-parked", None);
    registry.set_gc_block_state(parked.thread_id, Arc::clone(&parked.gc_block_state));
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    let stamp = frame.redefine_stamp();
    parked.frames.push(frame);
    for round in 1..=300 {
        // As `redefine_class_with` does, under the writer.
        let mut cm = shared.classes.class_manager.write();
        let floor = StaleFrameCensus::take(&shared).floor_of(cid);
        assert!(
            floor.is_some_and(|floor| floor <= stamp),
            "the parked frame holds the floor"
        );
        prune_histories_by_census(&shared, &mut cm);
        cm.redefine_class(cid, ldc_class(70_000 + round), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
    }
    let seen = cratonvm_classloading::class_redefinition_count();
    convert_obsolete_frames_if_redefined(&shared, &mut parked);
    assert!(parked.frames[0].runs_obsolete_method(), "translated");
    let floor = parked
        .gc_block_state
        .obsolete_frame_floor
        .load(std::sync::atomic::Ordering::Acquire);
    assert!(floor >= seen, "the woken thread publishes its pass");
    {
        let mut cm = shared.classes.class_manager.write();
        cm.prune_redefinition_histories(|_| Some(floor));
        let Some(history) = cm.redefinition_history(cid) else {
            unreachable!("the class was redefined");
        };
        assert_eq!(
            history.translate(stamp, 8),
            None,
            "nobody runs code that old any more: its steps are gone"
        );
    }
    let old = super::super::execute_frame(&shared, &mut parked);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the parked frame keeps its constant, got {old:?}"
    );
}

/// Interpreter round i1 wave 25, lane L3 (the host run of
/// `L3W24ParkedAcrossManyRenames.java` in the
/// default mode: `parked=parkQqA,parkQxC`). `retransformClasses`
/// redefines a class twice here -- back to its retransformation base,
/// then to the transformers' output (`instrument::native_retransform_classes0`)
/// -- and both halves move the renamed constant, so 600 retransforms of a
/// class a thread stays parked in are 1,199 history steps. The census kept
/// them past the soft caps, but the hard cap was 1,024 steps: the parked
/// frame's first steps were dropped and it read the new pool at its old
/// index (80600 here). `--compatible` hid it: its latch waits in 10 ms
/// slices, and each slice's blocking-region exit moves the frame, which
/// then never needs more than a few steps; the real JDK's latch parks once.
#[test]
fn a_thread_parked_across_six_hundred_retransforms_stays_translatable() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let original = ldc_class(70_000);
    let Some(cid) = define_probe(&shared, &original) else {
        return;
    };
    let mut parked = JvmThread::new(ThreadId(4_150), "obsolete-frames-parked-600");
    let registry = &shared.threads.thread_registry;
    registry.register(parked.thread_id, "obsolete-frames-parked-600", None);
    registry.set_gc_block_state(parked.thread_id, Arc::clone(&parked.gc_block_state));
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    let stamp = frame.redefine_stamp();
    parked.frames.push(frame);
    // Blocked in the frame, as its park's blocking deposit publishes it.
    publish_blocked_frame_classes(&parked);
    parked
        .gc_block_state
        .in_blocked_region
        .store(true, Ordering::Release);
    for round in 1..=600 {
        // As `native_retransform_classes0` does: the base, then the output.
        for bytes in [original.clone(), ldc_class(80_000 + round)] {
            let mut cm = shared.classes.class_manager.write();
            prune_histories_by_census(&shared, &mut cm);
            cm.redefine_class(cid, bytes, RedefineOptions::default())
                .expect("a same-shape redefinition succeeds");
        }
    }
    {
        let cm = shared.classes.class_manager.read();
        let Some(history) = cm.redefinition_history(cid) else {
            unreachable!("the class was redefined");
        };
        assert!(
            history.translate(stamp, 8).is_some(),
            "every step the parked frame needs is kept"
        );
    }
    // It wakes: the flag goes down, then its frames move.
    parked
        .gc_block_state
        .in_blocked_region
        .store(false, Ordering::Release);
    convert_obsolete_frames_if_redefined(&shared, &mut parked);
    let old = super::super::execute_frame(&shared, &mut parked);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the parked frame keeps its constant, got {old:?}"
    );
}

/// Interpreter round i1 wave 25, lane L3: a frame moved onto a private
/// copy of a replaced body decodes it into a stream kept with that body
/// (`ReplacedBody::decoded_stream`), not in the process-wide intern table,
/// whose entry holds the code it decoded -- which kept every such copy
/// allocated until its shard was cleared, long after the frame moved on
/// or was popped. And a frame of a class never redefined answers
/// `frame_runs_replaced_code` from the thread's memo once it has asked.
#[test]
fn a_moved_frame_decodes_its_code_outside_the_intern_table() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_152), "obsolete-frames-stream");
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    redefine_probe(&shared, cid, ldc_class(80_000));
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    let moved = &thread.frames[0];
    assert!(moved.runs_obsolete_method(), "moved onto a private copy");
    let Some(stream) = super::super::quickened_for_frame(moved) else {
        unreachable!("ldc; ireturn decodes");
    };
    assert!(
        Arc::ptr_eq(stream.code(), &moved.code),
        "the frame's own code"
    );
    assert!(
        super::super::quickened_for_frame(moved)
            .is_some_and(|again| Arc::ptr_eq(&again, &stream)),
        "built once, kept with the body"
    );
    assert!(
        !cratonvm_reader::quickened::intern(&moved.code)
            .is_some_and(|interned| Arc::ptr_eq(&interned, &stream)),
        "not the intern table's entry"
    );
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the moved frame keeps its constant, got {old:?}"
    );

    // A class with no history: asked once under the lock, then memoised.
    let never = ClassId::new(0x00F1_0C77);
    let other = Frame::new(
        never,
        "obsolete/Never".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        Vec::new(),
        1,
        1,
        &[],
    );
    assert!(!frame_runs_replaced_code(&shared, &other));
    let memoised = NO_HISTORY_SEEN.with(|ways| {
        ways.iter()
            .any(|way| way.get().0 == shared.vm_identity && way.get().1 == never.as_u32())
    });
    assert!(
        memoised,
        "the no-history answer is memoised for this thread"
    );
    assert!(!frame_runs_replaced_code(&shared, &other));
}

/// Interpreter round i1 wave 46, lane L3: the fenced-loop debug lines'
/// holder answer follows this thread's class-manager guard whenever
/// lock-order tracking records it (a debug test build), and is `None`
/// otherwise -- never the wrong way round.
#[test]
fn the_class_manager_holder_answer_follows_this_threads_guard() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    {
        let _reader = shared.classes.class_manager.read();
        assert_ne!(
            class_manager_held_here(),
            Some(false),
            "held while the guard lives"
        );
    }
    assert_ne!(
        class_manager_held_here(),
        Some(true),
        "released with the guard"
    );
}

/// Interpreter round i1 wave 24, lane L3 (the orchestrator's host runs of
/// `L3ObsoleteParkedAcrossToggles` / `...Renames` /
/// `...VirtualThreadResume`: a rare `parkQqA,parkQqB`): a thread whose
/// conversion finds the class-manager lock busy -- another thread defining
/// or redefining a class as it wakes -- used to give up until its next
/// safepoint or blocking-region exit, and its frame ran its old `ldc`
/// index against the new pool meanwhile: 80000 here. The conversion is
/// now retried at the dispatch loop's next top, before any bytecode runs.
#[test]
fn a_conversion_the_busy_class_manager_deferred_runs_before_the_next_bytecode() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let mut thread = JvmThread::new(ThreadId(4_141), "obsolete-frames-deferred");
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    redefine_probe(&shared, cid, ldc_class(80_000));
    {
        // Another thread's class definition, as this one leaves its
        // blocking region (`check_post_block_gc`).
        let _writer = shared.classes.class_manager.write();
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
    }
    assert!(thread.frames.conversion_pending(), "deferred");
    assert!(
        !thread.frames[0].runs_obsolete_method(),
        "not moved while the lock was busy"
    );
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the frame keeps its constant, got {old:?}"
    );
    assert!(!thread.frames.conversion_pending(), "retried and done");

    // A thawed continuation's frames, the same way
    // (`interpreter::resume_continuation`).
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    let frozen = frame.to_frozen_frame();
    redefine_probe(&shared, cid, ldc_class(90_000));
    let mut resumed = JvmThread::new(ThreadId(4_142), "obsolete-frames-deferred-thaw");
    resumed.frames.push(Frame::from_frozen_frame(frozen));
    {
        let _writer = shared.classes.class_manager.write();
        convert_thawed_frames(&shared, &mut resumed);
    }
    assert!(resumed.frames.conversion_pending(), "deferred");
    // Straight into the dispatch loop, as `resume_continuation` does after
    // its own `convert_thawed_frames`.
    let thawed = super::super::execute_frame(&shared, &mut resumed);
    assert!(
        matches!(thawed, Ok(Some(Value::Int(80_000)))),
        "the thawed frame keeps its constant, got {thawed:?}"
    );
}

/// Interpreter round i1 wave 38, lane L3 (the wave-37 regression of
/// `L6/RedefineRunningSpliceProbe`): a thread NONE of whose frames runs
/// the redefined class, whose conversion was deferred because the
/// redefining thread still held the writer (a withdrawn body's forced
/// poll takes it right then), clears the pending mark at its first retry
/// once the lock is free. Wave 37 retried through `convert_at_loop_top`,
/// whose "no frame of a redefined class" shortcut left the mark set, and
/// the loop ran no bytecode for the whole retry bound.
#[test]
fn a_deferred_conversion_that_owes_nothing_clears_at_the_next_top() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let unrelated = Frame::new(
        ClassId::new(0x00F1_0C79),
        "obsolete/Unrelated".to_string(),
        "spin".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        Vec::new(),
        1,
        1,
        &[],
    );
    let mut thread = JvmThread::new(ThreadId(4_160), "obsolete-frames-owes-nothing");
    thread.frames.push(unrelated);
    redefine_probe(&shared, cid, ldc_class(80_000));
    {
        // The redefining thread's writer, still held when this one polls.
        let _writer = shared.classes.class_manager.write();
        convert_obsolete_frames_if_redefined(&shared, &mut thread);
    }
    assert!(
        thread.frames.conversion_pending(),
        "deferred while the writer held the lock"
    );
    // The dispatch loop's next top, the lock free and no fence up.
    retry_deferred_conversion(&shared, &mut thread);
    assert!(
        !thread.frames.conversion_pending(),
        "one retry once the lock is free, not the whole retry bound"
    );
    assert_eq!(thread.frames.conversion_retries(), 0);
}

/// Interpreter round i1 wave 24, lane L3 (the i22-L3 census proposal's
/// wave-23 note): a thread blocked since before every redefinition --
/// an idle pool worker, a JDK service thread -- used to hold the ONE
/// census floor back for every class, so no history was ever trimmed
/// below its soft caps. Blocked with an exact list of its frames' classes
/// that does not name the class, it no longer counts for it; blocked in a
/// frame of the class, or under compiled code (an inexact list), or
/// running, it still does.
#[test]
fn a_thread_blocked_in_other_classes_does_not_hold_a_classs_history() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let mut idle = JvmThread::new(ThreadId(4_140), "obsolete-frames-idle");
    let registry = &shared.threads.thread_registry;
    registry.register(idle.thread_id, "obsolete-frames-idle", None);
    registry.set_gc_block_state(idle.thread_id, Arc::clone(&idle.gc_block_state));
    let idle_floor = idle
        .gc_block_state
        .obsolete_frame_floor
        .load(Ordering::Acquire);
    // Its blocking deposit, as `deposit_root_snapshot` makes it: no frame
    // of the probe class.
    publish_blocked_frame_classes(&idle);
    idle.gc_block_state
        .in_blocked_region
        .store(true, Ordering::Release);
    let stamp = cratonvm_classloading::class_redefinition_count();
    for round in 1..=20 {
        let mut cm = shared.classes.class_manager.write();
        prune_histories_by_census(&shared, &mut cm);
        cm.redefine_class(cid, ldc_class(70_000 + round), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
    }
    {
        let mut cm = shared.classes.class_manager.write();
        let census = StaleFrameCensus::take(&shared);
        assert!(
            census.floor_of(cid).is_some_and(|floor| floor > idle_floor),
            "the idle thread runs no frame of the class"
        );
        prune_histories_by_census(&shared, &mut cm);
        let Some(history) = cm.redefinition_history(cid) else {
            unreachable!("the class was redefined");
        };
        assert_eq!(
            history.translate(stamp, 8),
            None,
            "nobody runs code that old: past the last eight, its steps are gone"
        );
    }
    // Blocked in a frame of the class, it holds the class's floor.
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    idle.frames.push(frame);
    publish_blocked_frame_classes(&idle);
    assert_eq!(
        StaleFrameCensus::take(&shared).floor_of(cid),
        Some(idle_floor),
        "listed"
    );
    // An inexact list (the registry's native-block marker) counts for
    // every class, and so does a running thread.
    let _ = idle.frames.pop();
    publish_blocked_frame_classes(&idle);
    idle.gc_block_state.blocked_frame_classes.lock().exact = false;
    assert_eq!(
        StaleFrameCensus::take(&shared).floor_of(cid),
        Some(idle_floor)
    );
    publish_blocked_frame_classes(&idle);
    idle.gc_block_state
        .in_blocked_region
        .store(false, Ordering::Release);
    assert_eq!(
        StaleFrameCensus::take(&shared).floor_of(cid),
        Some(idle_floor)
    );
}

/// `value()`'s current body as a door's template, as a publish site
/// hands it to `deopt_resume::stamp_compiled_source_of`.
fn value_template(
    shared: &SharedVm,
    cid: ClassId,
) -> Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(cid)?;
    let method = class.methods.iter().find(|m| &*m.name == "value")?;
    let code = method.code()?;
    Some(Arc::new(
        crate::classloading::resolution::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: cid,
                class_name: Arc::clone(&class.name),
                method_name: Arc::from("value"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&code.code[..]),
                exception_table: Arc::from(Vec::<ExceptionTableEntry>::new()),
                max_stack: code.max_stack,
                max_locals: code.max_locals,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ),
    ))
}

fn body_built_now() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("a code buffer");
    buf.emit(&[0xC3]); // ret
    cratonvm_jit::CompiledMethod::new(buf)
}

/// Round 13 wave 3 (lane replay2; proposal R13-1): a trap out of a body
/// compiled before its class's redefinition resumes in the bytecode the
/// body was compiled from (its publish site's template), which the
/// class's history can move onto the new pool, and never in the door's
/// NEW template. Nothing to translate before any redefinition; a body
/// with no stamped source, or stamped with the door's own template,
/// keeps the door's template.
#[test]
fn a_body_of_a_redefined_class_resumes_in_its_own_source() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "obsolete/VmProbe",
            &ldc_class(70_000),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            Err(_) => return,
        }
    };
    let Some(old) = value_template(&shared, cid) else {
        return;
    };
    // Built with the VM's constant-pool stamp source registered: the
    // generation `old` indexes.
    let body = body_built_now();
    let unstamped = body_built_now();
    let Some(stamp) = body.compile_cp_stamp() else {
        return;
    };
    super::super::deopt_resume::stamp_compiled_source_of(&body, Arc::clone(&old));
    assert!(
        !rebuilt_body_translates(&shared, cid, &old.code, &old.exception_table, stamp),
        "a class with no history has nothing to translate from"
    );
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    let Some(new) = value_template(&shared, cid) else {
        return;
    };
    assert!(rebuilt_body_translates(
        &shared,
        cid,
        &old.code,
        &old.exception_table,
        stamp
    ));
    let chosen = super::super::deopt_resume::obsolete_activation_source(&shared, &body, &new);
    assert!(
        chosen.is_some_and(|src| Arc::ptr_eq(&src, &old)),
        "the body's own source, not the door's new template"
    );
    assert!(
        super::super::deopt_resume::obsolete_activation_source(&shared, &unstamped, &new)
            .is_none(),
        "no stamped source: the door's template, as before"
    );
    let own = body_built_now();
    super::super::deopt_resume::stamp_compiled_source_of(&own, Arc::clone(&new));
    assert!(
        super::super::deopt_resume::obsolete_activation_source(&shared, &own, &new).is_none(),
        "stamped with the door's own template: nothing changes"
    );
}

/// Interpreter round i1 wave 37, lane L3 (window 1 of
/// `interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001`): a
/// redefinition that gives an old index another constant raises a fence
/// that holds every other thread's dispatch loop at its top -- the loop's
/// slow path defers the thread's conversion instead of running a bytecode
/// -- and never its owner's. Once the fence is down the held loop moves
/// its frame before its next bytecode, and the frame reads its own
/// constant. An unchanged pool raises none.
#[test]
fn a_redefinition_fence_holds_other_loops_until_it_is_down() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    assert!(
        redefinition_moves_constants(&shared, cid, &ldc_class(80_000)),
        "#8 names 80000 in the new pool, 70000 in the old one"
    );
    assert!(
        !redefinition_moves_constants(&shared, cid, &ldc_class(70_000)),
        "the same pool moves nothing"
    );
    let barrier = &shared.mem.gc_barrier;
    let owner = ThreadId(4_151);
    let quiet = RedefinitionFence::raise(&shared, owner, cid, &ldc_class(70_000));
    assert!(
        !barrier.redefinition_fence_holds(ThreadId(4_150)),
        "no fence for a pool that moves nothing"
    );
    drop(quiet);

    let mut thread = JvmThread::new(ThreadId(4_150), "obsolete-frames-fenced");
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    let waits = barrier.redefinition_fence_waits();
    let fence = RedefinitionFence::raise(&shared, owner, cid, &ldc_class(80_000));
    assert!(barrier.redefinition_fence_holds(thread.thread_id));
    assert!(!barrier.redefinition_fence_holds(owner), "never its owner");
    redefine_probe(&shared, cid, ldc_class(80_000));
    // The held loop's top, as `interpreter::loop_top_poll` calls it.
    convert_at_loop_top(&shared, &mut thread);
    assert!(thread.frames.conversion_pending(), "held at its top");
    assert!(barrier.redefinition_fence_waits() > waits, "counted");
    assert!(
        !thread.frames[0].runs_obsolete_method(),
        "not moved while the fence is up"
    );
    // Its next top, still fenced: still pending, nothing run.
    retry_deferred_conversion(&shared, &mut thread);
    assert!(thread.frames.conversion_pending());
    drop(fence);
    assert!(!barrier.redefinition_fence_holds(thread.thread_id));
    retry_deferred_conversion(&shared, &mut thread);
    assert!(
        !thread.frames.conversion_pending(),
        "converted once the fence is down"
    );
    assert!(
        thread.frames[0].runs_obsolete_method(),
        "moved onto its own body"
    );
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the held frame keeps its constant, got {old:?}"
    );
}

/// Interpreter round i1 wave 37, lane L3
/// (`L3/L3W37RedefineTwoClassesAtOnce`): a call of two classes holds the
/// other threads from before its first install to after its last; the
/// fence of each class inside it adds nothing and takes it down with
/// nothing; a call of one class raises no call-wide fence.
#[test]
fn a_call_wide_fence_spans_its_classes() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let barrier = &shared.mem.gc_barrier;
    let owner = ThreadId(4_154);
    let other = ThreadId(4_155);
    let moving = ldc_class(80_000);
    let same = ldc_class(70_000);
    assert!(
        !raise_install_fence(&shared, owner, &[(cid, &moving[..])]),
        "one class: its own fence"
    );
    assert!(!barrier.redefinition_fence_holds(other));
    // Wave 38: a call none of whose classes moves a constant is still one
    // operation (the fence goes up), with no handshake.
    assert!(raise_install_fence(
        &shared,
        owner,
        &[(cid, &same[..]), (cid, &same[..])]
    ));
    assert!(barrier.redefinition_fence_holds(other));
    lower_install_fence(&shared, owner);
    assert!(!barrier.redefinition_fence_holds(other));
    assert!(raise_install_fence(
        &shared,
        owner,
        &[(cid, &same[..]), (cid, &moving[..])]
    ));
    assert!(barrier.redefinition_fence_holds(other));
    assert!(barrier.redefinition_fence_owned_by(owner));
    {
        // A class of the call that moves constants: held already.
        let class_fence = RedefinitionFence::raise(&shared, owner, cid, &ldc_class(80_000));
        drop(class_fence);
        assert!(
            barrier.redefinition_fence_holds(other),
            "the class's fence did not take the call's down"
        );
    }
    lower_install_fence(&shared, owner);
    assert!(!barrier.redefinition_fence_holds(other));
    assert!(!barrier.redefinition_fence_owned_by(owner));
}

/// Interpreter round i1 wave 37, lane L3 (the i19-L3 compiled-bodies
/// page's "inlined chain" item): a sink resuming an inlined chain out of a
/// body compiled before its class's redefinition pushes the OUTERMOST
/// frame from the old code beneath the chain's current inner frames.
/// Restamped at its index with the compile stamp, it is moved onto its
/// translated body and reads its own constant; the frame above it, current,
/// is left alone.
#[test]
fn the_outermost_frame_of_a_rebuilt_chain_reads_its_own_constants() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(4_153), "obsolete-frames-chain");
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let compile_stamp = cratonvm_classloading::class_redefinition_count();
    let Some(compiled_from) = value_frame(&shared, cid) else {
        return;
    };
    redefine_probe(&shared, cid, ldc_class(80_000));
    convert_obsolete_frames_if_redefined(&shared, &mut thread);
    // The chain as a sink pushes it: the outermost frame from the old code,
    // stamped now, and an inner frame of current code above it.
    let outermost = Frame::new_from_arcs(
        cid,
        Arc::from("obsolete/VmProbe"),
        Arc::from("value"),
        Arc::from("()I"),
        None,
        Arc::clone(&compiled_from.code),
        Arc::from(Vec::<ExceptionTableEntry>::new()),
        1,
        0,
        &[],
    );
    let Some(inner) = value_frame(&shared, cid) else {
        return;
    };
    let inner_code = Arc::clone(&inner.code);
    thread.frames.push(outermost);
    thread.frames.push(inner);
    stamp_frame_at_rebuilt_from_compiled_code(&shared, &mut thread, 0, Some(compile_stamp));
    assert!(
        thread.frames[0].runs_obsolete_method(),
        "the outermost frame is moved onto its translated body"
    );
    assert!(
        Arc::ptr_eq(&thread.frames[1].code, &inner_code),
        "the current inner frame keeps its code"
    );
    let _ = thread.frames.pop();
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "the outermost frame keeps its constant, got {old:?}"
    );
}

/// Interpreter round i1 wave 37, lane L3 (windows 2 and 4 of the same
/// page): a running thread's loop, sent to its slow path by the
/// redefinition (`GcBarrier::note_code_moved_on_every_loop`), moves its
/// frame there -- it used to wait for its next safepoint or blocking exit,
/// which a thread returning from a compiled callee frozen through the
/// handshake, or any thread after a redefinition that took no handshake,
/// may not reach for a long time.
#[test]
fn a_loop_top_moves_a_frame_of_a_redefined_class() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let mut thread = JvmThread::new(ThreadId(4_152), "obsolete-frames-loop-top");
    let Some(frame) = value_frame(&shared, cid) else {
        return;
    };
    thread.frames.push(frame);
    redefine_probe(&shared, cid, ldc_class(80_000));
    convert_at_loop_top(&shared, &mut thread);
    assert!(
        thread.frames[0].runs_obsolete_method(),
        "moved at the loop top"
    );
    let old = super::super::execute_frame(&shared, &mut thread);
    assert!(
        matches!(old, Ok(Some(Value::Int(70_000)))),
        "its own constant, got {old:?}"
    );
}

/// A frame of `value()` pushed from a cached-invoke entry built from the
/// body the class has now (`Frame::new_pooled_cached`), the entry naming
/// the constant-pool generation it was read at when `name_generation`
/// (as the invoke-cache fills do since wave 43).
fn cached_value_frame(shared: &SharedVm, cid: ClassId, name_generation: bool) -> Option<Frame> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(cid)?;
    let method = class.methods.iter().find(|m| &*m.name == "value")?;
    let code = method.code()?;
    let cached = crate::classloading::resolution::CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cid,
            class_name: Arc::clone(&class.name),
            method_name: Arc::from("value"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&code.code[..]),
            exception_table: Arc::from(Vec::<ExceptionTableEntry>::new()),
            max_stack: code.max_stack,
            max_locals: code.max_locals,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    );
    let cached = if name_generation {
        // Read under the guard the code was read under, as the fills do.
        cached.with_pool_generation(cratonvm_classloading::class_redefinition_count())
    } else {
        cached
    };
    drop(cm);
    Some(Frame::new_pooled_cached(
        Arc::new(cached),
        &[],
        &mut Vec::new(),
        &mut Vec::new(),
    ))
}

/// Interpreter round i1 wave 43, lane L2
/// (`i39-L2-a-frame-pushed-inside-a-constant-only-redefinition-reads-the-new-constant`,
/// through `interpreter-L3-proposal-frames-carry-the-pool-generation-their-door-validated-FIXED-20261011`):
/// a constant-only redefinition (`value()` is `ldc #8; ireturn` before
/// and after, `#8` 70000 then 80000). A frame pushed inside the swap from
/// an entry read before it carries a stamp inside the window and the
/// current bytes, which made the conversion take it for the new body
/// (80000). With the entry's pool generation trusted
/// (`FRAMES_TRUST_THEIR_BODYS_POOL_GENERATION`) it is translated as the
/// body its door chose and keeps its constant; an entry that names no
/// generation is converted as before.
#[test]
fn a_frame_pushed_inside_a_constant_only_swap_runs_the_body_its_door_chose() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(cid) = define_probe(&shared, &ldc_class(70_000)) else {
        return;
    };
    let (Some(mut named), Some(mut unnamed)) = (
        cached_value_frame(&shared, cid, true),
        cached_value_frame(&shared, cid, false),
    ) else {
        return;
    };
    let mut named_thread = JvmThread::new(ThreadId(4_243), "obsolete-frames-named-generation");
    let mut unnamed_thread = JvmThread::new(ThreadId(4_244), "obsolete-frames-no-generation");
    // The same code; only the constant at #8 changes.
    redefine_probe(&shared, cid, ldc_class(80_000));
    let began = {
        let cm = shared.classes.class_manager.read();
        let Some(history) = cm.redefinition_history(cid) else {
            unreachable!("the class was redefined");
        };
        history.latest_redefinition_began()        };
    // As a door that held no class-manager lock would have stamped them.
    named.restamp_redefined_body(began, false, None);
    unnamed.restamp_redefined_body(began, false, None);
    // A stamp inside the swap is stale for the OSR door either way.
    assert!(frame_runs_replaced_code(&shared, &named));
    assert!(frame_runs_replaced_code(&shared, &unnamed));

    named_thread.frames.push(named);
    convert_obsolete_frames_if_redefined(&shared, &mut named_thread);
    let named_value = super::super::execute_frame(&shared, &mut named_thread);
    let expected = if FRAMES_TRUST_THEIR_BODYS_POOL_GENERATION {
        70_000
    } else {
        80_000
    };
    assert!(
        matches!(named_value, Ok(Some(Value::Int(v))) if v == expected),
        "the entry's generation decides, got {named_value:?}"
    );

    unnamed_thread.frames.push(unnamed);
    convert_obsolete_frames_if_redefined(&shared, &mut unnamed_thread);
    let unnamed_value = super::super::execute_frame(&shared, &mut unnamed_thread);
    assert!(
        matches!(unnamed_value, Ok(Some(Value::Int(80_000)))),
        "no generation: taken for the new body as before, got {unnamed_value:?}"
    );
}
