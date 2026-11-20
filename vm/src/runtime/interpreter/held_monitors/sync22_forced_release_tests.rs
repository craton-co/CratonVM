use super::*;
use crate::config::VmConfig;
use crate::runtime::interpreter::jvmti_events::release_forced_frame_block_monitors;
use crate::threading::jvm_thread::JvmThread;
use crate::vm::SharedVm;

/// `synchronized (a) { a = null; ... }` resumed after the enter:
/// `aload_0 dup astore_1 monitorenter; aconst_null astore_0; aload_1
/// monitorexit; return`.
fn block_frame(pc: usize, lock: ObjectRef) -> Frame {
    let code = vec![0x2a, 0x59, 0x4c, 0xc2, 0x01, 0x4b, 0x2b, 0xc3, 0xb1];
    let mut f = Frame::new(
        cratonvm_types::ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        "(Ljava/lang/Object;)V".to_string(),
        None,
        code,
        Vec::new(),
        2,
        4,
        &[],
    );
    f.set_local(0, Value::Object(Some(lock)));
    f.set_local(1, Value::Object(Some(lock)));
    f.last_instr_pc = pc;
    f
}

#[test]
fn sync22_an_understated_frame_gives_back_the_lock_its_bytecode_holds() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(2201), "sync22-forced-release");
    let tid = thread.thread_id;
    let lock = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    shared.threads.monitors.enter(lock, tid);
    let mut f = block_frame(4, lock);
    f.held_monitors.mark_understated();
    thread.frames.push(f);
    assert_eq!(release_forced_frame_block_monitors(&shared, &mut thread, 0), (1, 0));
    assert!(!shared.threads.monitors.holds(lock, tid), "the leak of the i23 page");
}

#[test]
fn sync22_an_exact_record_a_frame_outside_its_block_and_a_recorded_lock_are_left_alone() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(2202), "sync22-forced-release-exact");
    let tid = thread.thread_id;
    let lock = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    // A caller's hold on the same object: one level the frame below does
    // not own, released only through ITS record.
    shared.threads.monitors.enter(lock, tid);
    // Not understated (an exact, empty record): nothing to add.
    thread.frames.push(block_frame(4, lock));
    assert_eq!(release_forced_frame_block_monitors(&shared, &mut thread, 0), (0, 0));
    assert!(shared.threads.monitors.holds(lock, tid));
    // Understated but before the enter: the bytecode holds nothing.
    thread.frames[0].held_monitors.mark_understated();
    thread.frames[0].last_instr_pc = 0;
    assert_eq!(release_forced_frame_block_monitors(&shared, &mut thread, 0), (0, 0));
    assert!(shared.threads.monitors.holds(lock, tid));
    // Understated and recorded: the record releases it once, nothing more.
    thread.frames[0].last_instr_pc = 4;
    thread.frames[0].held_monitors.push(lock);
    assert_eq!(release_forced_frame_block_monitors(&shared, &mut thread, 0), (1, 0));
    assert!(!shared.threads.monitors.holds(lock, tid));
}

/// mon21: a caller's recorded hold on the same object is another level;
/// the understated frame above gives back ITS level only when the table
/// holds one beyond the records (compiled code's lock), and not when the
/// table holds exactly what the caller's record names (the level the
/// bytecode claims was never taken: an elided nested lock).
#[test]
fn mon21_an_unrecorded_level_is_released_only_beyond_what_the_records_cover() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(2203), "mon21-forced-release-levels");
    let tid = thread.thread_id;
    let lock = shared.mem.heap.alloc_object(cratonvm_types::ClassId::new(7), 0);
    // The caller (frame 0) records its one level; the frame above is
    // understated and its bytecode holds `lock` too.
    let mut caller = block_frame(0, lock);
    caller.held_monitors.push(lock);
    thread.frames.push(caller);
    let mut top = block_frame(4, lock);
    top.held_monitors.mark_understated();
    thread.frames.push(top);

    // The table holds only the caller's level: nothing beyond the
    // records, so the claimed level is withheld (counted as refused) and the
    // caller's level is not released.
    shared.threads.monitors.enter(lock, tid);
    assert_eq!(release_forced_frame_block_monitors(&shared, &mut thread, 1), (0, 1));
    assert_eq!(shared.threads.monitors.entry_count(lock), 1);
    assert!(!table_holds_beyond_records(&shared, &thread, lock));

    // Compiled code's own level on top of it: now exactly one is released.
    shared.threads.monitors.enter(lock, tid);
    assert!(table_holds_beyond_records(&shared, &thread, lock));
    assert_eq!(release_forced_frame_block_monitors(&shared, &mut thread, 1), (1, 0));
    assert_eq!(shared.threads.monitors.entry_count(lock), 1, "the caller's level stays");
    assert!(shared.threads.monitors.holds(lock, tid));
}
