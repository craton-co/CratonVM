// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The must-be-zero half of `published_code_free_audit()`.
//!
//! `published_code_free_audit().1` counts published compiled bodies whose
//! executable memory reached the OS **without** passing through the retirement
//! queue. Three documents call it an invariant in those words —
//! `vm/src/jit/code_cache_lifecycle.rs` ("That counter must be zero"),
//! `docs/jit/code-cache-lifecycle.md:338` ("is the check, and it must stay
//! zero") and `docs/jit/code-cache-lifetime.md` — and it is the backstop for
//! the whole "ownership, not the quiescence walk, is the primary argument"
//! claim: a new `Arc<CompiledMethod>` holder added by someone who does not know
//! the obligation exists is exactly what the counter is for.
//!
//! Until this file existed the counter's only consumer was
//! `the_free_audit_records_a_release_that_skipped_the_queue` in
//! `jit/src/tests.rs`, which asserts the counter **can increment** — the
//! opposite direction. Nothing anywhere asserted it stays at zero, so a
//! regression that started bypassing the queue on the ordinary path would have
//! flipped an invariant three documents describe and failed nothing.
//!
//! # Why this is a separate integration binary
//!
//! Both counters are process-global and monotone. `jit/src/tests.rs`'s test
//! deliberately drives the unqueued counter up, and a sibling in the same
//! process can only ever observe it higher. An integration test is its own
//! process, so "zero" is a statement about this file's own work and nothing
//! else's — which is the only way a must-be-zero assertion can be written.
//!
//! Keep this file free of any deliberate bypass: a test that needs one belongs
//! in the unit-test binary beside the increment test that already has one.

use std::sync::Arc;

use cratonvm_jit::{
    jit_execution_enter, jit_execution_leave, lookup_jit_code_range, published_code_free_audit,
    register_jit_code_range, CompiledMethod, ExecutableBuffer, JitCache,
};

/// Poll `cond`, giving the retirement queue the quiescence observations it
/// drains on. Mirrors `eventually_drained` in `jit/src/tests.rs`: the queue is
/// drained by threads entering and leaving JIT execution, not by a timer, so a
/// bare sleep would never make progress.
fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if cond() {
            return true;
        }
        jit_execution_leave(jit_execution_enter());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    cond()
}

/// Publish a body into `cache` and register its code range, exactly as a real
/// install does, and hand back its entry address.
fn publish(cache: &JitCache, class: &Arc<str>, method: &Arc<str>, desc: &Arc<str>) -> usize {
    let cid = cratonvm_types::ClassId::new(1);
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let cm = cache
        .get(class, method, desc, cid)
        .expect("the body must be published");
    let entry = cm.entry_ptr() as usize;
    register_jit_code_range(entry, cm.code_len(), Arc::as_ptr(&cm) as usize);
    entry
}

/// The ordinary install → invalidate → reclaim cycle, many times over, must
/// release every body **through the queue**.
///
/// This is the assertion the invariant never had. It is deliberately not a
/// check that the bodies were freed at all — `the_last_owner_releases_once_jit
/// _execution_is_quiescent` in the unit tests owns that question. What is owned
/// here is the *route*: `unqueued` must still read zero when the work is done.
#[test]
fn every_published_body_released_on_the_ordinary_path_went_through_the_queue() {
    let (_, unqueued_at_start) = published_code_free_audit();
    assert_eq!(
        unqueued_at_start, 0,
        "this binary must start clean, or the assertion below is about \
         somebody else's release. If this fires, something ELSE in this test \
         binary released a published body outside the retirement queue before \
         this test ran — which is the bug the counter exists to name.",
    );

    let cache = JitCache::new();
    let desc: Arc<str> = Arc::from("()V");
    let mut entries = Vec::new();
    for i in 0..32 {
        let class: Arc<str> = Arc::from(format!("FreeAuditClass{i}").as_str());
        let method: Arc<str> = Arc::from("m");
        entries.push((
            publish(&cache, &class, &method, &desc),
            class.clone(),
            method.clone(),
        ));
    }

    // Invalidate them all. `remove` routes the artifact through the retirement
    // queue rather than dropping it where it stands.
    for (_, class, method) in &entries {
        cache.remove(class, method, &desc, cratonvm_types::ClassId::new(1));
    }

    let drained = eventually(|| {
        entries
            .iter()
            .all(|(entry, ..)| lookup_jit_code_range(*entry).is_none())
    });
    let (freed, unqueued) = published_code_free_audit();

    assert_eq!(
        unqueued, 0,
        "a published compiled body reached the OS without passing through the \
         retirement queue. `docs/jit/code-cache-lifecycle.md` states this \
         counter must stay zero; a non-zero reading means some owner of an \
         `Arc<CompiledMethod>` drops it directly instead of deferring it, and \
         a thread executing inside that body at the moment of the drop \
         executes unmapped memory.",
    );
    assert!(
        drained,
        "every body should have been reclaimed once JIT execution was \
         quiescent (freed so far: {freed})",
    );
    assert!(
        freed > 0,
        "nothing was freed at all — the zero above would then be vacuous",
    );
}

/// The same invariant across a thread that is inside JIT execution while the
/// invalidation happens: the release must be *deferred*, never *skipped*.
///
/// Deferral is the interesting arm because it is the one where an artifact
/// outlives its cache entry, which is exactly the window in which an
/// unqueued drop is possible.
#[test]
fn a_release_deferred_by_an_executing_thread_still_goes_through_the_queue() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("FreeAuditDeferredClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let entry = publish(&cache, &class, &method, &desc);

    // Hold the process inside JIT execution across the invalidation, so the
    // queue cannot release while this token is live.
    let token = jit_execution_enter();
    cache.remove(&class, &method, &desc, cratonvm_types::ClassId::new(1));
    assert!(
        lookup_jit_code_range(entry).is_some(),
        "a body must not be unmapped while a thread is inside compiled code",
    );
    jit_execution_leave(token);

    assert!(
        eventually(|| lookup_jit_code_range(entry).is_none()),
        "and must be released once JIT execution is quiescent",
    );
    assert_eq!(
        published_code_free_audit().1,
        0,
        "the deferred release must still be a QUEUED release — a deferral that \
         ends in a direct drop is the same unmapping bug with a delay",
    );
}
