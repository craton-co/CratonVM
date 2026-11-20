//! gc-common w36-f (handoff-w36b): a native's object return handed back to
//! compiled code does not stay a GC root. See
//! [`drop_native_return_handed_to_compiled_code`].
use super::*;
use crate::config::VmConfig;
use crate::threading::ThreadId;
use std::sync::Arc;

/// Restores the JIT thread pointer even if an assertion panics while it is
/// installed (wave-8 lesson (c)).
struct JitThreadInstalled(Option<JitThreadScope>);
impl Drop for JitThreadInstalled {
    fn drop(&mut self) {
        if let Some(scope) = self.0.take() {
            restore_jit_thread(scope);
        }
    }
}

#[test]
fn a_fast_native_object_return_is_not_rooted_after_the_helper_returns() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let mut thread = Box::new(JvmThread::new(ThreadId(0), "w36f-native-return"));
    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let leftover = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    thread.java_thread_obj = Some(mirror);
    // A value an EARLIER native call returned to compiled code: the loop
    // shape of `WeakClearYoungProbe`, where it was the referent.
    thread.native_pending_return = Some(leftover);

    let installed = JitThreadInstalled(Some(set_jit_thread(&mut thread)));
    // SAFETY: `vm_ptr` names the live `SharedVm` above and the JIT thread
    // pointer is installed for this OS thread -- the contract compiled
    // code satisfies when it calls the helper.
    let bits = unsafe { jit_thread_current_thread_direct(vm_ptr) };
    drop(installed);

    assert_eq!(
        bits,
        mirror.as_ptr() as i64,
        "the fast arm answers the thread's mirror (Compatible mode, no gate)"
    );
    assert!(
        thread.native_pending_return.is_none(),
        "a native's object return handed to compiled code must not stay a \
         GC root until the next native call (handoff-w36b)"
    );
}

#[test]
fn the_hand_off_drops_the_slot_and_nothing_else() {
    let mut thread = JvmThread::new(ThreadId(0), "w36f-drop");
    // SAFETY: non-null and 8-byte aligned (`ObjectRef::from_raw`'s whole
    // contract); nothing dereferences them.
    let (ret, exc) = unsafe {
        (
            ObjectRef::from_raw(0x20_0000usize as *mut u8),
            ObjectRef::from_raw(0x20_0040usize as *mut u8),
        )
    };
    thread.native_pending_return = Some(ret);
    thread.jit_pending_exception = Some(exc);
    drop_native_return_handed_to_compiled_code(&mut thread);
    assert!(thread.native_pending_return.is_none());
    assert_eq!(
        thread.jit_pending_exception,
        Some(exc),
        "a parked exception is its own root and must survive the hand-off"
    );
}

/// Source witness: every extern helper that can return a native's object
/// to compiled code drops the handoff root on the way out. A wrapper that
/// loses the call re-opens the leak silently: nothing crashes, a dropped
/// object just stays reachable.
#[test]
fn every_object_returning_native_door_hands_the_root_off() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    for helper in [
        "jit_invoke_dispatch(",
        "jit_invoke_virtual_mic(",
        "jit_indy_bridge(",
        "jit_varhandle_read_direct<const SLOT: usize>(",
        "jit_integer_value_of_direct(",
        "jit_long_value_of_direct(",
        "jit_thread_current_thread_direct(",
        "jit_concurrent_hashmap_get_direct(",
        "jit_hashmap_get_direct(",
        "jit_string_latin1_to_lower_direct(",
        "jit_hashmap_put_direct(",
    ] {
        let sig = format!("extern \"C\" fn {helper}");
        let start = src
            .find(&sig)
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start..];
        let body = &rest[..rest.find("\n}\n").map_or(rest.len(), |e| e + 3)];
        assert!(
            body.contains("native_return_handed_to_compiled_code();"),
            "`{helper}` returns a native's object to compiled code without \
             dropping native_pending_return (handoff-w36b)"
        );
    }
}
