use super::{door_will_enter_memoized_wrapped_body, WrappedBodyMemo, WRAPPED_BODY_MEMO};
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use std::sync::Arc;

fn template(is_static: bool) -> Arc<cratonvm_jit_api::CachedBytecodeMethod> {
    Arc::new(cratonvm_jit_api::CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cratonvm_types::ClassId::new(78),
            class_name: Arc::from("r11w7/Door"),
            method_name: Arc::from("incSync"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(vec![0xB1u8, 0, 0].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: true,
            is_static,
        },
    ))
}

/// Metadata only; the `ret` body never runs.
fn body(has_indy_trap: bool) -> Arc<cratonvm_jit::CompiledMethod> {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut compiled = cratonvm_jit::CompiledMethod::new(buf);
    compiled.has_indy_trap = has_indy_trap;
    Arc::new(compiled)
}

fn remember(
    template: &Arc<cratonvm_jit_api::CachedBytecodeMethod>,
    generation: u64,
    body: Option<&Arc<cratonvm_jit::CompiledMethod>>,
) {
    WRAPPED_BODY_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        memo.clear();
        memo.push(WrappedBodyMemo {
            template: Arc::downgrade(template),
            generation,
            body: body.map(Arc::downgrade).unwrap_or_default(),
        });
    });
}

/// Proposal 23: the nomination door stops counting a `static
/// synchronized` template only when this thread's memo already says the
/// monitored door will ENTER its wrapped body at the current generation.
/// Every other answer keeps the count, and none of them looks anything up.
#[test]
fn counting_stops_only_for_a_memoized_body_the_door_enters() {
    let shared = SharedVm::new(VmConfig::default());
    let thread = JvmThread::new(ThreadId(0), "r11w7-door-count");
    let generation = shared.jit.jit_cache.generation();
    let stat = template(true);
    let entered = body(false);

    WRAPPED_BODY_MEMO.with(|memo| memo.borrow_mut().clear());
    assert!(
        !door_will_enter_memoized_wrapped_body(&shared, &thread, &stat),
        "an empty memo is a miss, not a lookup"
    );

    remember(&stat, generation, Some(&entered));
    assert!(door_will_enter_memoized_wrapped_body(
        &shared, &thread, &stat
    ));

    remember(&stat, generation.wrapping_add(1), Some(&entered));
    assert!(
        !door_will_enter_memoized_wrapped_body(&shared, &thread, &stat),
        "a stale generation may name a withdrawn body"
    );

    remember(&stat, generation, None);
    assert!(
        !door_will_enter_memoized_wrapped_body(&shared, &thread, &stat),
        "a memoized 'no wrapped body' keeps the count"
    );

    let refused = body(true);
    remember(&stat, generation, Some(&refused));
    assert!(
        !door_will_enter_memoized_wrapped_body(&shared, &thread, &stat),
        "a body the door refuses leaves the template interpreted, so it is counted"
    );

    let dropped = body(false);
    remember(&stat, generation, Some(&dropped));
    drop(dropped);
    assert!(!door_will_enter_memoized_wrapped_body(
        &shared, &thread, &stat
    ));

    // Round 16 (lane sync): since round 12 wave 8 the door ENTERS an
    // instance template's wrapped body too (under
    // `CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY`, default on), so it stops
    // counting exactly as for a static one. This assertion used to read
    // `false` ("interpreted"), and passed only because the peek handed
    // the door `num_params`, without the receiver.
    let inst = template(false);
    remember(&inst, generation, Some(&entered));
    assert_eq!(
        door_will_enter_memoized_wrapped_body(&shared, &thread, &inst),
        crate::runtime::env_cache::jit_door_sync_instance_body(),
        "an instance template the door enters stops counting, as a static one does"
    );
    remember(&inst, generation, None);
    assert!(
        !door_will_enter_memoized_wrapped_body(&shared, &thread, &inst),
        "an instance template with no wrapped body keeps the count"
    );
    WRAPPED_BODY_MEMO.with(|memo| memo.borrow_mut().clear());
}
