use super::{pin_decoded_jit_args, pinned_decoded_args, write_back_pinned_decoded_args};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::{ObjectRef, Value};

fn fake_ref(word: &mut [u64; 2]) -> ObjectRef {
    // SAFETY: non-null and 8-aligned; never dereferenced — the pin window
    // only stores the address and hands it back.
    unsafe { ObjectRef::from_raw(word.as_mut_ptr() as *mut u8) }
}

/// The decoded doors' handler-frame locals must come from the pin window,
/// not from the pre-call copy: a collection that relocated a reference
/// argument during the compiled call rewrites the pin, and the locals must
/// follow it. The guard must release exactly its own window and leave a
/// pin that was already below it alone (the synchronized-method monitor
/// pin sits there).
#[test]
fn pinned_decoded_args_follow_a_relocated_reference() {
    let mut thread = JvmThread::new(ThreadId(0), "r9-vmside-pin");
    let mut a = [0u64; 2];
    let mut b = [0u64; 2];
    let mut c = [0u64; 2];
    let before = fake_ref(&mut a);
    let after = fake_ref(&mut b);
    let monitor = fake_ref(&mut c);
    thread.native_pin_roots.push(monitor);
    let base = thread.native_pin_roots.len();
    let args = [
        Value::Int(7),
        Value::Object(Some(before)),
        Value::Object(None),
        Value::Long(-1),
    ];
    let mut pins = [usize::MAX; 8];
    {
        let _guard = pin_decoded_jit_args(&mut thread, &args, &mut pins);
        assert_eq!(
            pins[..4],
            [usize::MAX, base, usize::MAX, usize::MAX],
            "only the non-null reference is pinned"
        );
        // Stand-in for the collector's remap of `native_pin_roots`.
        thread.native_pin_roots[base] = after;
        let locals = pinned_decoded_args(&thread, &args, &pins);
        assert_eq!(locals[0], Value::Int(7));
        assert_eq!(locals[1], Value::Object(Some(after)));
        assert_eq!(locals[2], Value::Object(None));
        assert_eq!(locals[3], Value::Long(-1));
    }
    assert_eq!(
        thread.native_pin_roots.len(),
        base,
        "the guard releases exactly the window it opened"
    );
    assert_eq!(thread.native_pin_roots[0], monitor);
}

/// r9w2-vm2: a post-call decline writes the pinned (relocated) references
/// back into the caller's buffer, which the caller then re-runs the method
/// interpreted from. Non-references and nulls are left alone.
#[test]
fn declined_decoded_call_writes_relocated_args_back() {
    let mut thread = JvmThread::new(ThreadId(0), "r9w2-vm2-writeback");
    let mut a = [0u64; 2];
    let mut b = [0u64; 2];
    let before = fake_ref(&mut a);
    let after = fake_ref(&mut b);
    let mut caller = [
        Value::Object(Some(before)),
        Value::Int(3),
        Value::Object(None),
    ];
    let mut pins = [usize::MAX; 8];
    let guard = pin_decoded_jit_args(&mut thread, &caller, &mut pins);
    let pin = pins[0];
    // Stand-in for the collector's remap of `native_pin_roots`.
    thread.native_pin_roots[pin] = after;
    write_back_pinned_decoded_args(&thread, &mut caller, &pins);
    assert_eq!(caller[0], Value::Object(Some(after)));
    assert_eq!(caller[1], Value::Int(3));
    assert_eq!(caller[2], Value::Object(None));
    drop(guard);
    assert!(thread.native_pin_roots.is_empty());
}

/// More arguments than pin slots must not panic: the extra reference is
/// simply not pinned and comes back unchanged.
#[test]
fn pin_slots_shorter_than_args_do_not_panic() {
    let mut thread = JvmThread::new(ThreadId(0), "r9-vmside-pin-short");
    let mut a = [0u64; 2];
    let obj = fake_ref(&mut a);
    let args = [Value::Object(Some(obj)), Value::Object(Some(obj))];
    let mut pins = [usize::MAX; 1];
    let guard = pin_decoded_jit_args(&mut thread, &args, &mut pins);
    assert_eq!(thread.native_pin_roots.len(), 1);
    let locals = pinned_decoded_args(&thread, &args, &pins);
    assert_eq!(
        locals,
        vec![Value::Object(Some(obj)), Value::Object(Some(obj))]
    );
    drop(guard);
    assert!(thread.native_pin_roots.is_empty());
}
