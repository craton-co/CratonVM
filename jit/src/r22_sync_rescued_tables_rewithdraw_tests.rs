use super::*;
use std::sync::atomic::Ordering;

fn published_body() -> Arc<CompiledMethod> {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    let _ = cm._buffer.mark_published();
    Arc::new(cm)
}

/// A caller whose side tables were moved out (as a rescue does) and whose
/// MIC slot is then re-published with callee `D` as its only owner; the
/// tables are dropped while this thread is inside compiled code. Answers
/// whether `D` is still a live artifact afterwards (queued, not freed).
fn republished_callee_survives_the_tables_release() -> bool {
    let token = jit_execution_enter();
    let mut caller = {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]);
        CompiledMethod::new(buf)
    };
    caller._jit_mic_slots.push(Box::new(JitMICSlot::new()));
    let tables = RescuedSideTables::take_from(&mut caller);
    drop(caller);
    let callee = published_body();
    let weak = Arc::downgrade(&callee);
    {
        // The late publication by a thread still inside the rescued code.
        let slot = &tables.mic_slots[0];
        let mut owner = slot.compiled_owner.lock();
        slot.cached_entry_word
            .store(callee.entry_ptr() as u64, Ordering::SeqCst);
        *owner = Some(callee);
    }
    drop(tables);
    let alive = weak.upgrade().is_some();
    jit_execution_leave(token);
    alive
}

#[test]
fn a_late_published_owner_leaves_a_rescued_table_through_the_queue() {
    assert!(republished_callee_survives_the_tables_release());
}

