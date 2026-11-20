use super::door_declines_not_entrant_body;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::sync::Arc;

extern "C" fn never_called(_record: i64, _regs: i64, _stack: i64) -> i64 {
    0
}

/// `NOP5; MOV EAX, 7; RET`, a static `()I` body with the entry pad.
fn padded_body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&cratonvm_jit::ENTRY_PATCH_PAD);
    buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]);
    let mut body = cratonvm_jit::CompiledMethod::new(buf);
    body.entry_java_args = 0;
    body
}

#[test]
fn a_door_declines_a_not_entrant_body_and_enters_a_live_one() {
    let cache = cratonvm_jit::JitCache::new();
    cache.arm_not_entrant(cratonvm_jit::NotEntrantHook {
        helper: never_called as usize,
        vm_ptr: 0,
    });
    let class = cratonvm_types::ClassId::new(0x1717);
    let (c, m, d) = ("r16w17/Unbox", "val", "()I");
    cache.put(
        Arc::from(c),
        Arc::from(m),
        Arc::from(d),
        class,
        padded_body(),
    );
    let stale = cache.get(c, m, d, class).expect("published");
    let mut thread = JvmThread::new(ThreadId(0), "test");
    assert!(!door_declines_not_entrant_body(&mut thread, &stale, "test"));
    cache.remove(c, m, d, class);
    assert_eq!(
        cache.make_not_entrant_after_eviction(std::slice::from_ref(&stale)),
        1
    );
    assert!(door_declines_not_entrant_body(&mut thread, &stale, "test"));
    // The recompiled body the stub would forward to is entered.
    cache.put(
        Arc::from(c),
        Arc::from(m),
        Arc::from(d),
        class,
        padded_body(),
    );
    let fresh = cache.get(c, m, d, class).expect("republished");
    assert!(!door_declines_not_entrant_body(&mut thread, &fresh, "test"));
    drop(cratonvm_jit::RetainedCode::new(stale));
    drop(cratonvm_jit::RetainedCode::new(fresh));
}
