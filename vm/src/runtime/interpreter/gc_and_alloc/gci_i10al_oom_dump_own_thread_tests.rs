use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};

/// gci i10/al (`gci-i10al-oome-dump-gc-safe-slice-re-derives-mut-jvmthread-FIXED-20261016`):
/// the OOME report takes the caller's `&mut JvmThread` down to the GC-safe
/// slice; nothing on that path re-derives one from a raw pointer.
#[test]
fn gci_i10al_oom_dump_slice_borrows_the_callers_thread() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let body = |name: &str| -> String {
        let start = src.find(name).expect(name);
        let rest = &src[start..];
        rest[..rest.find("\n}\n").expect("end")].to_string()
    };
    for (f, sig) in [
        (
            "\npub(crate) fn maybe_dump_heap_on_oom(",
            "thread: &mut JvmThread",
        ),
        (
            "\npub(crate) fn maybe_dump_heap_on_oom_for(",
            "thread: &mut JvmThread",
        ),
        ("fn write_heap_dump_on_oom(", "thread: &mut JvmThread"),
        // zt t31/x10: the dump itself, shared with the full-collection dumps.
        ("fn write_heap_dump_now(", "thread: &mut JvmThread"),
        ("fn oom_dump_gc_safe_slice(", "thread: &mut JvmThread"),
    ] {
        let b = body(f);
        let head = &b[..b.find('{').expect("body")];
        assert!(head.contains(sig), "{f}: {sig}");
        assert!(
            !b.contains("current_jvm_thread_of("),
            "{f}: no re-derived thread"
        );
        assert!(!b.contains("unsafe {"), "{f}: no raw-pointer borrow");
    }
    let w = body("fn write_heap_dump_now(");
    let check = w
        .find("oom_dump_thread_is_registered(shared, thread)")
        .expect("own check");
    let slice = w
        .find("oom_dump_gc_safe_slice(shared, &mut *thread)")
        .expect("reborrowed slice");
    let retry = w
        .find("dump_heap_on_oom(&arc, &path, tid, gzip, &mut slice)")
        .expect("retrying dump");
    let single = w
        .find("crate::runtime::hprof::dump_heap_framed(&arc, &path, tid, gzip)")
        .expect("single request");
    assert!(check < slice && slice < retry && retry < single);
}

/// gci i10/al: only the registry's own `JvmThread` for its id opens
/// slices; a free-standing one (never published, or published at another
/// address, or dead) takes the single request.
#[test]
fn gci_i10al_oom_dump_slices_only_on_the_registered_thread() {
    let shared = crate::vm::SharedVm::new(VmConfig::default());
    let tid = ThreadId(4242);
    let thread = JvmThread::new(tid, "i10al-oom");
    assert!(
        !oom_dump_thread_is_registered(&shared, &thread),
        "unknown id"
    );
    let registry = &shared.threads.thread_registry;
    registry.register(tid, "i10al-oom", None);
    assert!(
        !oom_dump_thread_is_registered(&shared, &thread),
        "not published"
    );
    registry.set_jvm_thread_addr(tid, (&thread as *const JvmThread as usize) ^ 0x8);
    assert!(
        !oom_dump_thread_is_registered(&shared, &thread),
        "another address"
    );
    registry.set_jvm_thread_addr(tid, &thread as *const JvmThread as usize);
    assert!(
        oom_dump_thread_is_registered(&shared, &thread),
        "the published thread"
    );
    registry.mark_dead(tid);
    assert!(!oom_dump_thread_is_registered(&shared, &thread), "dead");
}
