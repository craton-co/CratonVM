use super::*;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodId};

const OWNER: &str = "cratonvm/test/SyncNativeEntry";

/// `1` when the calling thread holds the monitor of `args[0]` (the
/// receiver), `0` otherwise.
fn holds_receiver(
    ctx: &mut dyn NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    let held = match args.first() {
        Some(cratonvm_types::Value::Object(Some(obj))) => ctx.current_thread_holds_lock(*obj),
        _ => false,
    };
    Ok(Some(cratonvm_types::Value::Int(i32::from(held))))
}

fn vm_with_native() -> (Arc<SharedVm>, NativeMethodId) {
    let mut vm = SharedVm::new(crate::config::VmConfig::default());
    vm.natives.native_methods.register(
        OWNER,
        "held",
        "()I",
        holds_receiver,
        NativeKind::Bridge,
    );
    let id = vm
        .natives
        .native_methods
        .resolve_id(OWNER, "held", "()I")
        .expect("registered above");
    (Arc::new(vm), id)
}

/// Serve one cached `invokespecial` of `OWNER.held()I` on a fresh receiver
/// and return what the native answered, and whether the monitor is still
/// held afterwards.
fn serve(sync: Option<(bool, ClassId)>) -> (i32, bool) {
    let (shared, id) = vm_with_native();
    let caller = ClassId::new(91_003);
    let mut thread = JvmThread::new(ThreadId(4_748), "sync-native-entry");
    thread.frames.push(Frame::new(
        caller,
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    let receiver = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    thread.frames[0]
        .stack
        .push(cratonvm_types::Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes one reference");
    // The invoke cache retires its entries when the PROCESS-wide class
    // redefinition count moves (`InvokeCache::retire_after_redefinition`
    // reads `class_redefinition_count()` outside the classloading crate's
    // own tests), and an unattributed bump empties every cache. Another
    // test of a parallel run that redefines or bumps between the `put`
    // and the lookup turns the planted entry into a `CacheMiss`
    // (`docs/feature-designs/real-jdk-mode.md`).
    // Plant it again when the count moved; a miss with a still count is
    // a real failure.
    let mut attempts = 0;
    let outcome = loop {
        let redefinitions_before = crate::classloading::class_redefinition_count();
        // A lookup first brings the cache up to the count (`put` stamps
        // the entry with the count the cache last saw, which a move since
        // `JvmThread::new` would make stale).
        let _ = thread.invoke_cache.get(caller, 5, true, 0);
        thread.invoke_cache.put(
            caller,
            5,
            true,
            0,
            CachedInvokeTarget::Native {
                callback: holds_receiver,
                native_id: id,
                native_kind: NativeKind::Bridge,
                num_params: 0,
                facts: cratonvm_jit_api::DescriptorFacts::of("()I"),
                is_static: false,
                sync,
                gate: RedefineGate::never_stale(),
                owner: ClassId::new(0),
            },
        );
        let outcome = execute_invokevirtual_cached(&shared, &mut thread, 0, 5, 0, true, false);
        let raced = matches!(outcome, Ok(CachedCallResult::CacheMiss))
            && crate::classloading::class_redefinition_count() != redefinitions_before;
        if raced && attempts < 16 {
            attempts += 1;
            continue;
        }
        break outcome;
    };
    assert!(
        matches!(outcome, Ok(CachedCallResult::Handled)),
        "the cached entry serves the call: {outcome:?}"
    );
    let answer = thread.frames[0]
        .stack
        .pop_int()
        .expect("the native's int answer was pushed");
    let still_held = shared.threads.monitors.holds(receiver, thread.thread_id);
    (answer, still_held)
}

#[test]
fn a_synchronized_entry_holds_the_receiver_monitor_around_the_native() {
    let (answer, still_held) = serve(Some((false, ClassId::new(0))));
    assert_eq!(answer, 1, "the native ran under the receiver's monitor");
    assert!(
        !still_held,
        "the monitor is released when the native returns"
    );
}

#[test]
fn a_plain_entry_takes_no_monitor() {
    let (answer, still_held) = serve(None);
    assert_eq!(
        answer, 0,
        "no monitor for a method that is not synchronized"
    );
    assert!(!still_held);
}

#[test]
fn the_fill_fact_follows_the_method_flags() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let declaring = ClassId::new(7);
    assert_eq!(cached_native_sync(&shared, false, true, declaring), None);
    if native_sync_enabled(&shared) {
        assert_eq!(
            cached_native_sync(&shared, true, true, declaring),
            Some((true, declaring))
        );
        assert_eq!(
            cached_native_sync(&shared, true, false, declaring),
            Some((false, declaring))
        );
    } else {
        assert_eq!(cached_native_sync(&shared, true, true, declaring), None);
    }
}
