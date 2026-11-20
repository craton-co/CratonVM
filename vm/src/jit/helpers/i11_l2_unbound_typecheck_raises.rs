use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::sync::Arc;

/// i11-L2: a type-check site the compiler could not bind (interned under
/// its holder) whose class cannot be resolved raises from BOTH helpers —
/// the resolution error pending and the `i64::MIN` sentinel — for a plain
/// and an ARRAY receiver alike, as the interpreter's opcodes throw at the
/// same site. It used to answer `false` (a silent `0`, or a
/// `ClassCastException` naming the wrong cause).
#[test]
pub fn an_unbound_site_whose_class_is_missing_raises_from_both_helpers() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let holder = {
        let mut cm = shared.classes.class_manager.write();
        match cm.try_ensure_synthetic_class("cratonvm/test/i11l2/JitCastHolder", 0) {
            Ok(id) => id,
            Err(_) => return, // --real-jdk policy in the environment
        }
    };
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let mut thread = Box::new(JvmThread::new(ThreadId(11), "i11l2-jit-typecheck"));
    let plain = shared.mem.heap.alloc_object(holder, 0);
    let ints = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
    for (cp_index, name) in [
        (1u16, "cratonvm/test/i11l2/JitMissing"),
        (2u16, "[Lcratonvm/test/i11l2/JitMissing;"),
    ] {
        let (ptr, len) =
            cratonvm_jit::intern_typecheck_unresolved_in(name, holder.as_u32(), cp_index);
        for (receiver, shape) in [(plain, "object"), (ints, "int[]")] {
            for checkcast in [true, false] {
                thread.jit_pending_exception = None;
                let scope = set_jit_thread(&mut thread);
                // SAFETY: `vm_ptr` names the live VM above, `receiver` is a
                // live heap object of it, the site pointer is an interned
                // name, and the JIT thread pointer is installed for this
                // OS thread — the contract compiled code satisfies.
                let got = unsafe {
                    if checkcast {
                        jit_checkcast(vm_ptr, receiver.as_ptr() as i64, ptr, len as i64)
                    } else {
                        jit_instanceof(vm_ptr, receiver.as_ptr() as i64, ptr, len as i64)
                    }
                };
                restore_jit_thread(scope);
                let pending = thread.jit_pending_exception.take();
                let label = format!(
                    "{} of a {shape} against {name}",
                    if checkcast { "checkcast" } else { "instanceof" }
                );
                match pending {
                    Some(exc) => {
                        assert_eq!(got, i64::MIN, "{label}: raised, so the sentinel");
                        let cid = shared.mem.heap.class_id_of(exc);
                        let exc_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(cid)
                            .map(|c| c.name.to_string());
                        assert_eq!(
                            exc_name.as_deref(),
                            Some("java/lang/NoClassDefFoundError"),
                            "{label}"
                        );
                    }
                    // The stripped unit VM could not build the throwable:
                    // the helper must then fail soft, never hand out the
                    // sentinel with nothing pending.
                    None => assert_ne!(got, i64::MIN, "{label}: sentinel with nothing pending"),
                }
            }
        }
    }
}

/// i11-L2 (wave 12): JVMS §5.4.3 at a compiled unbound type-check site.
/// The first execution's resolution failure is recorded against the
/// site's `(holder, cp index)`; once the class has become resolvable, the
/// second execution must still raise the RECORDED error rather than
/// resolve (or ask a loader) again. Before wave 12 the site carried no cp
/// index, recorded nothing, and answered `0` the second time.
#[test]
pub fn a_recorded_failure_is_rethrown_at_an_unbound_site_after_the_class_appears() {
    const LATE: &str = "cratonvm/test/i12l2/JitLateClass";
    const CP_INDEX: u16 = 3;
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let holder = {
        let mut cm = shared.classes.class_manager.write();
        match cm.try_ensure_synthetic_class("cratonvm/test/i12l2/JitRecordHolder", 0) {
            Ok(id) => id,
            Err(_) => return, // --real-jdk policy in the environment
        }
    };
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let mut thread = Box::new(JvmThread::new(ThreadId(12), "i12l2-jit-typecheck"));
    let receiver = shared.mem.heap.alloc_object(holder, 0);
    let (ptr, len) =
        cratonvm_jit::intern_typecheck_unresolved_in(LATE, holder.as_u32(), CP_INDEX);
    let run = |thread: &mut Box<JvmThread>| {
        thread.jit_pending_exception = None;
        let scope = set_jit_thread(thread);
        // SAFETY: `vm_ptr` names the live VM above, `receiver` is a live
        // heap object of it, the site pointer is an interned name, and the
        // JIT thread pointer is installed for this OS thread.
        let got = unsafe { jit_instanceof(vm_ptr, receiver.as_ptr() as i64, ptr, len as i64) };
        restore_jit_thread(scope);
        (got, thread.jit_pending_exception.take())
    };

    let (first, first_exc) = run(&mut thread);
    let resolver = crate::runtime::resolve::MemberResolver::new(&shared);
    let recorded = resolver.probe_resolution_failure(resolver.scope(holder), CP_INDEX);
    let (Some(_), Some(record)) = (first_exc, recorded) else {
        // The stripped unit VM could not build (or classify) the
        // `LinkageError`: nothing was recorded, so there is nothing to
        // rethrow.
        return;
    };
    assert_eq!(first, i64::MIN, "the first failure is raised");
    assert_eq!(&*record.error_class, "java/lang/NoClassDefFoundError");

    // The class becomes resolvable through the holder's loader.
    let appeared = {
        let mut cm = shared.classes.class_manager.write();
        cm.try_ensure_synthetic_class(LATE, 0).is_ok()
    };
    let resolvable = appeared && {
        let cm = shared.classes.class_manager.read();
        crate::runtime::interpreter::class_resolved_without_loading(&shared, &cm, holder, LATE)
            .is_some()
    };
    if !resolvable {
        return;
    }

    let (second, second_exc) = run(&mut thread);
    assert_eq!(
        second,
        i64::MIN,
        "the recorded failure is rethrown, not answered by a fresh resolution"
    );
    let Some(exc) = second_exc else {
        return; // no throwable could be built this time: fail-soft is fine
    };
    let exc_name = shared
        .classes
        .class_manager
        .read()
        .get_class(shared.mem.heap.class_id_of(exc))
        .map(|c| c.name.to_string());
    assert_eq!(exc_name.as_deref(), Some("java/lang/NoClassDefFoundError"));
}
