//! JIT round 19 proposal wave (lane throws19, TP16-15;
//! `CRATONVM_JIT_LOCAL_HANDLER_LEAN=1`): the lean arm of
//! [`jit_local_handler_lookup`] answers exactly as the default arm -- the
//! same candidate, the same throwable handed to the handler slot, the same
//! site cache -- on a miss, a hit, a miss of another class, and a re-miss.
use super::*;
use crate::threading::ThreadId;
use cratonvm_jit::JitLocalHandlerSite;

/// Restores the JIT thread pointer even if an assertion panics while it
/// is installed.
struct Installed(Option<JitThreadScope>);
impl Drop for Installed {
    fn drop(&mut self) {
        if let Some(scope) = self.0.take() {
            restore_jit_thread(scope);
        }
    }
}

fn site(candidates: Vec<(&'static str, u32)>, declaring: ClassId) -> JitLocalHandlerSite {
    JitLocalHandlerSite {
        candidates,
        declaring_class_id: declaring.as_u32(),
        throw_bci: 0,
        cache: std::sync::atomic::AtomicU64::new(JitLocalHandlerSite::CACHE_EMPTY),
        implicit_cache: JitLocalHandlerSite::empty_implicit_cache(),
        no_collect: false,
    }
}

/// One compiled catch of `exc` at `site`: `(answer, handler slot, the
/// pending throwable was taken)`.
fn catch_once(
    shared: &SharedVm,
    site: &JitLocalHandlerSite,
    exc: ObjectRef,
) -> (i64, i64, bool) {
    let mut thread = Box::new(JvmThread::new(ThreadId(0), "r19-local-handler-lean"));
    thread.jit_pending_exception = Some(exc);
    let installed = Installed(Some(set_jit_thread(&mut thread)));
    let mut out = 0i64;
    // SAFETY: `shared` and `site` outlive the call, `out` is a live local
    // standing in for the handler's frame slot, and the JIT thread pointer
    // is installed for this OS thread -- the contract the local-handler
    // stub satisfies.
    let answer = unsafe {
        jit_local_handler_lookup_body(
            shared as *const SharedVm as i64,
            site as *const JitLocalHandlerSite as i64,
            &mut out,
            &std::cell::Cell::new(DeferredTrapSnapshot::None),
        )
    };
    drop(installed);
    (answer, out, thread.jit_pending_exception.is_none())
}

#[test]
fn the_lean_arm_answers_as_the_default_arm() {
    const EXC: &str = "cratonvm/test/R19LeanHandlerExc";
    const OTHER: &str = "cratonvm/test/R19LeanHandlerOther";
    for lean in [false, true] {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        shared
            .threads
            .throwable_layout_cache
            .latch_local_handler_lean_for_test(lean);
        let (exc_class, other_class) = {
            let mut cm = shared.classes.class_manager.write();
            (
                cm.try_ensure_synthetic_class(EXC, 0)
                    .expect("Compatible mode fabricates"),
                cm.try_ensure_synthetic_class(OTHER, 0)
                    .expect("Compatible mode fabricates"),
            )
        };
        let exc = shared.mem.heap.alloc_object(exc_class, 0);
        let other = shared.mem.heap.alloc_object(other_class, 0);
        let site = site(vec![(EXC, 10), ("", 20)], exc_class);
        let cache = || {
            JitLocalHandlerSite::decode_cache(
                site.cache.load(std::sync::atomic::Ordering::Relaxed),
            )
        };
        // A miss on an empty cache, then a hit on the same class.
        for round in 0..2 {
            let (answer, slot, taken) = catch_once(&shared, &site, exc);
            assert_eq!(answer, 0, "lean={lean} round={round}: the typed row");
            assert_eq!(
                slot,
                exc.as_ptr() as i64,
                "lean={lean}: the throwable handed over"
            );
            assert!(taken, "lean={lean}: the pending throwable is taken");
            assert_eq!(cache(), Some((exc_class.as_u32(), 0)));
        }
        // Another class misses the cache (the validated read) and takes
        // the catch-all; the cache follows it.
        let (answer, slot, taken) = catch_once(&shared, &site, other);
        assert_eq!(answer, 1, "lean={lean}: the catch-all row");
        assert_eq!(slot, other.as_ptr() as i64);
        assert!(taken);
        assert_eq!(cache(), Some((other_class.as_u32(), 1)));
        // And the first class misses back.
        let (answer, _, _) = catch_once(&shared, &site, exc);
        assert_eq!(
            answer, 0,
            "lean={lean}: a re-miss resolves the typed row again"
        );
        assert_eq!(cache(), Some((exc_class.as_u32(), 0)));
    }
}
