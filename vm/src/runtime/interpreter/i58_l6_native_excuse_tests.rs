use super::*;
use crate::threading::jvm_thread::{NativeExcuse, ThreadId, NX_EXCUSED};

/// Java a native runs lowers the thread's native-excuse word while it
/// runs (a nested entry finds it lowered and keeps nothing) and raises it
/// again after; its lowering waits while a handshake pause holds the
/// excusal.
#[test]
fn java_a_native_runs_lowers_the_native_excuse_while_it_runs() {
    let mut thread = JvmThread::new(ThreadId(0x0F58), "w58-native-excuse");
    let raised = NativeExcuse::raised(true, 3);
    thread.gc_block_state.native_excuse.raise(raised);
    thread.native_excuse_raised = raised;
    {
        let outer = NestedJavaAck::enter(&thread);
        assert_eq!(outer.excuse, raised);
        assert_eq!(
            thread.gc_block_state.native_excuse.load(),
            0,
            "its Java runs"
        );
        {
            let inner = NestedJavaAck::enter(&thread);
            assert_eq!(inner.excuse, 0, "already lowered");
            assert!(inner.state.is_none());
        }
        assert_eq!(thread.gc_block_state.native_excuse.load(), 0);
    }
    assert_eq!(
        thread.gc_block_state.native_excuse.load(),
        raised,
        "back in the native"
    );
    // Excused by a pause: the entry waits for the release.
    assert_eq!(
        thread.gc_block_state.native_excuse.try_excuse(true),
        Some(raised)
    );
    let state = std::sync::Arc::clone(&thread.gc_block_state);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(state.native_excuse.load(), raised | NX_EXCUSED);
        state.native_excuse.release();
    });
    let t0 = std::time::Instant::now();
    {
        let guard = NestedJavaAck::enter(&thread);
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(20),
            "waited"
        );
        assert_eq!(guard.excuse, raised);
    }
    releaser.join().unwrap();
    assert_eq!(thread.gc_block_state.native_excuse.load(), raised);
}

/// The frame stack's one relocation lowers the word around the move
/// (waiting while a pause holds it), republishes the buffer's address,
/// and raises the word again.
#[test]
fn a_relocating_frame_stack_republishes_its_buffer_to_the_excuse_word() {
    let cell = std::sync::Arc::new(NativeExcuse::new());
    let mut frames = crate::runtime::frame::FrameStack::new();
    frames.set_excuse_cell(&cell);
    frames.reserve_stable(1);
    let first = cell.frames();
    assert_eq!(first, frames.as_ptr() as usize);
    let raised = NativeExcuse::raised(true, 1);
    cell.raise(raised);
    assert_eq!(cell.try_excuse(true), Some(raised));
    let c = std::sync::Arc::clone(&cell);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        c.release();
    });
    let t0 = std::time::Instant::now();
    // Far past the initial capacity: a relocation.
    frames.reserve_stable(1 << 12);
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(20),
        "waited"
    );
    releaser.join().unwrap();
    assert_eq!(cell.frames(), frames.as_ptr() as usize);
    assert_ne!(cell.frames(), first, "the buffer moved");
    assert_eq!(cell.load(), raised, "raised again after the move");
}
