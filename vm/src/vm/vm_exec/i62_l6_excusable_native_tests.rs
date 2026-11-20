use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId, NX_IN, NX_LISTABLE};

/// A row of `jni_native_frames` (a C agent's lambda proxy row, a JNI
/// native's) leaves the thread listable; the frame-trace reader never
/// reads the rows. Before wave 62 any row raised `NX_IN` alone.
#[test]
fn i62_l6_a_native_row_leaves_the_thread_listable() {
    let shared = SharedVm::new(crate::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(62), "i62-l6-rows");
    thread
        .jni_native_frames
        .push((0, ClassId::new(0x8000_0062), 1 << 62, 0));
    let span = enter_excusable_native(&mut thread, &shared.mem.gc_barrier, quiet_cb())
        .expect("a platform thread");
    assert_eq!(span.raised & (NX_IN | NX_LISTABLE), NX_IN | NX_LISTABLE);
    leave_excusable_native(&mut thread, span);
    assert_eq!(thread.gc_block_state.native_excuse.load(), 0);
}

fn quiet_native(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(None)
}

// A different body: identical code folding must not merge the two.
fn other_native(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

/// The callbacks as `enter_excusable_native` takes them.
fn quiet_cb() -> usize {
    quiet_native as NativeCallback as usize
}

fn other_cb() -> usize {
    other_native as NativeCallback as usize
}

/// `CRATONVM_HANDSHAKE_NATIVE_CALLBACK` (wave 62): the raised word's
/// callback is recorded, a native called directly by a native records its
/// own, and each leave puts back the one before it. The switch is per VM,
/// read at construction: driven through the cell itself here.
#[test]
fn i62_l6_the_excuse_word_carries_its_natives_callback() {
    let shared = SharedVm::new(crate::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(63), "i62-l6-callback");
    let barrier = &shared.mem.gc_barrier;
    let on = barrier.excused_native_callback();
    let outer =
        enter_excusable_native(&mut thread, barrier, quiet_cb()).expect("a platform thread");
    let cell = std::sync::Arc::clone(&thread.gc_block_state);
    let quiet = quiet_native as NativeCallback as usize;
    let other = other_native as NativeCallback as usize;
    assert_eq!(cell.native_excuse.callback(), if on { quiet } else { 0 });
    let inner =
        enter_excusable_native(&mut thread, barrier, other_cb()).expect("a platform thread");
    assert_eq!(cell.native_excuse.callback(), if on { other } else { 0 });
    leave_excusable_native(&mut thread, inner);
    assert_eq!(
        cell.native_excuse.callback(),
        if on { quiet } else { 0 },
        "the outer's again"
    );
    assert_eq!(cell.native_excuse.load(), outer.raised);
    leave_excusable_native(&mut thread, outer);
    assert_eq!(cell.native_excuse.callback(), 0);
    // The cell's own protocol, whatever the switch.
    assert_eq!(cell.native_excuse.replace_callback(quiet), 0);
    assert_eq!(cell.native_excuse.replace_callback(other), quiet);
    cell.native_excuse.restore_callback(quiet);
    assert_eq!(cell.native_excuse.callback(), quiet);
}

/// The i53 retirement: a reflective call no longer makes the thread
/// unlisted. The word says its rows were recorded (`NX_REFLECTIVE`), the
/// record holds them before the raise, and a native entered after the
/// call ended raises no `NX_REFLECTIVE` (the record is then stale and
/// unread). Before, the word carried `NX_IN` alone.
#[test]
fn i53_a_reflective_call_is_recorded_for_the_pause() {
    use crate::threading::jvm_thread::NX_REFLECTIVE;
    let shared = SharedVm::new(crate::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(531), "i53-reflective");
    let row = crate::runtime::stackwalker::ReflectiveCallRow {
        interp_depth: 0,
        jit_depth: 0,
        kind: cratonvm_native_api::ReflectiveCallKind::MethodInvoke,
        check: None,
        raised: None,
        door_save: None,
        adapter_pin: None,
    };
    thread.reflective_calls.push(row);
    let barrier = &shared.mem.gc_barrier;
    let span = enter_excusable_native(&mut thread, barrier, quiet_cb()).expect("excusable");
    assert_eq!(
        span.raised & (NX_IN | NX_LISTABLE | NX_REFLECTIVE),
        NX_IN | NX_LISTABLE | NX_REFLECTIVE
    );
    assert_eq!(
        thread.gc_block_state.native_rows.lock().reflective_calls,
        vec![row]
    );
    leave_excusable_native(&mut thread, span);
    thread.reflective_calls.clear();
    let span = enter_excusable_native(&mut thread, barrier, quiet_cb()).expect("excusable");
    assert_eq!(span.raised & (NX_LISTABLE | NX_REFLECTIVE), NX_LISTABLE);
    leave_excusable_native(&mut thread, span);
    assert_eq!(thread.gc_block_state.native_excuse.load(), 0);
}

/// The i53 retirement: a virtual thread is excusable and listable (its
/// continuation is frozen only after a yielding native returned). Before,
/// `enter_excusable_native` answered `None` for it.
#[test]
fn i53_a_virtual_thread_is_excusable() {
    let shared = SharedVm::new(crate::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(532), "i53-virtual");
    thread.kind = crate::threading::ThreadKind::Virtual;
    let span = enter_excusable_native(&mut thread, &shared.mem.gc_barrier, quiet_cb())
        .expect("a virtual thread is excusable");
    assert_eq!(span.raised & (NX_IN | NX_LISTABLE), NX_IN | NX_LISTABLE);
    leave_excusable_native(&mut thread, span);
    assert_eq!(thread.gc_block_state.native_excuse.load(), 0);
}
