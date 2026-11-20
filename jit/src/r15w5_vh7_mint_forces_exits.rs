use super::*;
use std::sync::atomic::Ordering::SeqCst;

/// A body with one (fake) exit poll site, so the cache registers it for
/// forcing; the force itself refuses the fake site, which is all a unit
/// test can publish.
fn forceable() -> Option<CompiledMethod> {
    let mut buf = ExecutableBuffer::new(64)?;
    buf.emit(&[0xC3]); // ret
    let mut cm = CompiledMethod::new(buf);
    cm.exit_poll_sites = vec![0];
    Some(cm)
}

/// The first-mint sweep marks every body it withdrew (the thin-bound
/// body and its raw caller) withdrawn, so a frame already running one
/// leaves at its next exit; a body it kept is not marked. Under a stand-in
/// dependency name: the real one's superseded half would reach another
/// test's superseded thin-bound body through the process-wide registry.
#[test]
fn the_first_mint_sweep_marks_what_it_withdrew() {
    if !varhandle_mint_forces_exits_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    const STAND_IN: &str = "R15Vh7/ThinBindStandIn";
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(0x7701);
    let (Some(mut bound), Some(mut caller), Some(plain)) =
        (forceable(), forceable(), forceable())
    else {
        return;
    };
    bound.inlined_methods.push((
        STAND_IN.to_string(),
        "<thin-bind>".to_string(),
        "()V".to_string(),
    ));
    cache.put("R15Vh7".into(), "loop".into(), "()V".into(), cid, bound);
    let bound = cache
        .get("R15Vh7", "loop", "()V", cid)
        .expect("the thin-bound body is published");
    caller._direct_callee_entries = vec![bound.entry_ptr() as usize];
    cache.put("R15Vh7".into(), "caller".into(), "()V".into(), cid, caller);
    let caller = cache
        .get("R15Vh7", "caller", "()V", cid)
        .expect("the raw caller is published");
    cache.put("R15Vh7".into(), "plain".into(), "()V".into(), cid, plain);
    let plain = cache
        .get("R15Vh7", "plain", "()V", cid)
        .expect("the plain body is published");
    let (withdrawn, _forced) = cache.invalidate_forcing_exits(STAND_IN);
    assert_eq!(withdrawn, 2, "the thin-bound body and its raw caller");
    assert!(bound.retired.load(SeqCst) && bound.is_withdrawn_by_redefinition());
    assert!(caller.retired.load(SeqCst) && caller.is_withdrawn_by_redefinition());
    assert!(
        !plain.is_withdrawn_by_redefinition(),
        "a body the sweep kept"
    );
    assert!(cache.get("R15Vh7", "plain", "()V", cid).is_some());
    assert!(cache.bodies_withdrawn_by_redefinition() >= 2);
}

/// VH6-1 (opt-in): a class-change invalidation retires a superseded body
/// that inlined from the class and withdraws its published raw caller;
/// off, it keeps both (the wave-4 reach).
#[test]
fn a_class_change_retires_superseded_dependents_when_asked() {
    for on in [false, true] {
        let mut cache = JitCache::new();
        cache.invalidation_retires_superseded = on;
        let cid = cratonvm_types::ClassId::new(0x7703);
        let (Some(mut dependent), Some(fresh), Some(mut caller)) =
            (forceable(), forceable(), forceable())
        else {
            return;
        };
        dependent.inlined_methods.push((
            "R15Vh7/Dep".to_string(),
            "value".to_string(),
            "()I".to_string(),
        ));
        cache.put("R15Vh7".into(), "dep".into(), "()V".into(), cid, dependent);
        let old = cache
            .get("R15Vh7", "dep", "()V", cid)
            .expect("the dependent body is published");
        cache.put("R15Vh7".into(), "dep".into(), "()V".into(), cid, fresh);
        assert!(old.superseded.load(SeqCst));
        caller._direct_callee_entries = vec![old.entry_ptr() as usize];
        cache.put("R15Vh7".into(), "caller".into(), "()V".into(), cid, caller);
        if cache.get("R15Vh7", "caller", "()V", cid).is_none() {
            return; // the caller was refused at publication here
        }
        cache.invalidate_for_class_change("R15Vh7/Dep");
        assert_eq!(old.retired.load(SeqCst), on);
        assert_eq!(cache.get("R15Vh7", "caller", "()V", cid).is_none(), on);
        assert!(
            cache.get("R15Vh7", "dep", "()V", cid).is_some(),
            "the fresh body stays"
        );
    }
}

/// G1: `put_osr` marks the OSR body it displaces `superseded`.
#[test]
fn put_osr_marks_the_displaced_body_superseded() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(0x7702);
    let (Some(mut first), Some(mut second)) = (forceable(), forceable()) else {
        return;
    };
    first.compiled_via_osr = true;
    second.compiled_via_osr = true;
    cache.put_osr("R15Vh7".into(), "osr".into(), "()V".into(), cid, first);
    let Some(first) = cache.get_osr("R15Vh7", "osr", "()V", cid) else {
        return; // refused at publication in this configuration
    };
    assert!(!first.superseded.load(SeqCst));
    cache.put_osr("R15Vh7".into(), "osr".into(), "()V".into(), cid, second);
    let replaced = cache
        .get_osr("R15Vh7", "osr", "()V", cid)
        .is_some_and(|now| !Arc::ptr_eq(&now, &first));
    assert_eq!(
        first.superseded.load(SeqCst),
        replaced,
        "the displaced OSR body, and only a displaced one, is superseded"
    );
}
