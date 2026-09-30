// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 12, wave 4, lane `withdraw` — tier proposal W3-1.
//!
//! Every door that withdraws a body from a [`JitCache`] armed with its VM's
//! tiered manager (`install_withdrawal_sink`) must put the methods it left
//! without a body back at `CompilationTier::Interpreter`. Before wave 4 only the
//! sweeper and the redefinition paths did, by hand; the class-hierarchy door,
//! the define door, the unload door and a deopt eviction's reverse closure left
//! a C2 method settled over an empty cache
//! (`r12w3-tier-inlining-invalidation-leaves-withdrawn-methods-settled-patch`).
//!
//! Each case builds its own cache and manager: a withdrawal logs an
//! invalidation record, which would refuse a later publication in a shared
//! cache.
//!
//! **Read, not executed by its author.** Lane `withdraw` may not build or run
//! anything; the orchestrator owns this file's first run.

use cratonvm_jit::tiered::{CompilationTier, MethodKey, TieredCompilationManager};
use cratonvm_jit::{CompiledMethod, ExecutableBuffer, JitCache, UnloadedClassSet};
use cratonvm_types::ClassId;
use std::sync::Arc;

const DESC: &str = "()V";
const C1: CompilationTier = CompilationTier::C1;
const C2: CompilationTier = CompilationTier::C2;
const INTERPRETED: CompilationTier = CompilationTier::Interpreter;

/// A cache armed with its own manager, and the class id its bodies use.
struct Fixture {
    cache: JitCache,
    manager: TieredCompilationManager,
    id: ClassId,
}

/// A fresh armed fixture, or `None` when the kill switch
/// `CRATONVM_JIT_WITHDRAWAL_DEMOTES=0` is set in this process's environment.
fn armed(id: u32) -> Option<Fixture> {
    let manager = TieredCompilationManager::with_default_policy();
    let cache = JitCache::new();
    if !cache.install_withdrawal_sink(manager.withdrawal_sink()) {
        return None;
    }
    assert!(cache.reports_withdrawals());
    Some(Fixture {
        cache,
        manager,
        id: ClassId::new(id),
    })
}

/// A one-`RET` body that records `inlined` as its class dependencies.
fn body(inlined: &[&str]) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]);
    let mut compiled = CompiledMethod::new(buf);
    compiled.inlined_methods = inlined
        .iter()
        .map(|class| ((*class).to_string(), "m".to_string(), DESC.to_string()))
        .collect();
    compiled
}

/// Publish `compiled` under `(class, method)` and record it at `tier`, as a
/// compile door and its completion would.
fn publish(
    f: &Fixture,
    class: &str,
    method: &str,
    tier: CompilationTier,
    compiled: CompiledMethod,
) -> MethodKey {
    let key = MethodKey::with_class_id(f.id, class, method, DESC);
    f.manager.on_method_invocation(&key);
    f.manager.compilation_complete(&key, tier, 1);
    let (c, m, d) = (Arc::from(class), Arc::from(method), Arc::from(DESC));
    f.cache.put(c, m, d, f.id, compiled);
    assert!(f.cache.get(class, method, DESC, f.id).is_some(), "published");
    assert_eq!(f.manager.current_tier(&key), tier);
    key
}

#[test]
fn a_class_hierarchy_change_demotes_the_caller_that_inlined_from_the_class() {
    let Some(f) = armed(0x0412_0001) else {
        return;
    };
    let hot = publish(&f, "w4/Caller", "hot", C2, body(&["w4/Shape"]));
    let kept = publish(&f, "w4/Caller", "kept", C2, body(&[]));

    assert_eq!(f.cache.invalidate_for_class_change("w4/Shape"), 1);

    assert_eq!(
        f.manager.current_tier(&hot),
        INTERPRETED,
        "a C2 method whose body a CHA change withdrew must be offered again"
    );
    assert_eq!(
        f.manager.current_tier(&kept),
        C2,
        "an unrelated body keeps its tier"
    );
}

#[test]
fn a_define_over_an_inlined_class_demotes_its_inliners() {
    let Some(f) = armed(0x0412_0002) else {
        return;
    };
    let hot = publish(&f, "w4/Definer", "hot", C1, body(&["w4/Helper"]));

    assert_eq!(f.cache.invalidate_for_class("w4/Helper"), 1);

    assert_eq!(f.manager.current_tier(&hot), INTERPRETED);
}

#[test]
fn an_unload_demotes_the_surviving_caller_that_inlined_the_unloaded_class() {
    let Some(f) = armed(0x0412_0003) else {
        return;
    };
    let survivor = publish(&f, "w4/Survivor", "hot", C2, body(&["w4/Unloaded"]));

    let classes = UnloadedClassSet::new([(ClassId::new(0x0412_0004), "w4/Unloaded")]);
    assert_eq!(f.cache.invalidate_unloaded_classes(&classes), 1);

    assert_eq!(f.manager.current_tier(&survivor), INTERPRETED);
}

/// `remove` is the deopt eviction. The deopted method itself is lowered by
/// `on_deoptimization` in production; the caller that baked a direct call
/// into it is withdrawn by the reverse closure, and nothing lowered it.
#[test]
fn a_deopt_eviction_demotes_the_caller_its_closure_withdrew() {
    let Some(f) = armed(0x0412_0005) else {
        return;
    };
    let callee = publish(&f, "w4/Deopt", "callee", C2, body(&[]));
    let entry = f
        .cache
        .get("w4/Deopt", "callee", DESC, f.id)
        .map(|cm| cm.entry_ptr() as usize);
    let mut baked = body(&[]);
    baked._direct_callee_entries = entry.into_iter().collect();
    let caller = publish(&f, "w4/Deopt", "caller", C2, baked);

    f.cache.remove("w4/Deopt", "callee", DESC, f.id);

    assert!(
        f.cache.get("w4/Deopt", "caller", DESC, f.id).is_none(),
        "the closure withdrew the caller"
    );
    assert_eq!(f.manager.current_tier(&callee), INTERPRETED);
    assert_eq!(
        f.manager.current_tier(&caller),
        INTERPRETED,
        "a closure member loses its body like the candidate, and its tier with it"
    );
}

/// A withdrawal that took only a method's OSR body leaves its method-entry
/// body live, and the tier names that body: lowering it would let a C1 task
/// publish over a live C2 body.
#[test]
fn an_osr_only_withdrawal_keeps_the_tier_of_the_live_method_entry_body() {
    let Some(f) = armed(0x0412_0006) else {
        return;
    };
    let key = publish(&f, "w4/Loop", "run", C2, body(&[]));
    let mut osr = body(&["w4/OsrOnly"]);
    osr.compiled_via_osr = true;
    let (c, m, d) = (Arc::from("w4/Loop"), Arc::from("run"), Arc::from(DESC));
    f.cache.put_osr(c, m, d, f.id, osr);
    assert!(f.cache.get_osr("w4/Loop", "run", DESC, f.id).is_some());

    assert_eq!(f.cache.invalidate_for_class_change("w4/OsrOnly"), 1);

    assert!(f.cache.get_osr("w4/Loop", "run", DESC, f.id).is_none());
    assert!(f.cache.get("w4/Loop", "run", DESC, f.id).is_some());
    assert_eq!(f.manager.current_tier(&key), C2);
}

/// The full flush demotes through the cache too, so `JitRealm`'s own
/// demotion can stand down; a second arming is refused; and a sink whose
/// manager is gone is skipped rather than reached.
#[test]
fn the_flush_demotes_the_first_sink_wins_and_a_dead_sink_is_skipped() {
    let Some(f) = armed(0x0412_0007) else {
        return;
    };
    let key = publish(&f, "w4/Flush", "run", C1, body(&[]));
    let other = TieredCompilationManager::with_default_policy();
    assert!(
        !f.cache.install_withdrawal_sink(other.withdrawal_sink()),
        "the first sink wins"
    );

    let (count, withdrawn) = f.cache.clear_all_collecting();
    assert_eq!((count, withdrawn.len()), (1, 1));
    assert_eq!(f.manager.current_tier(&key), INTERPRETED);

    let orphan = JitCache::new();
    let gone = TieredCompilationManager::with_default_policy();
    assert!(orphan.install_withdrawal_sink(gone.withdrawal_sink()));
    drop(gone);
    let (c, m, d) = (Arc::from("w4/Orphan"), Arc::from("run"), Arc::from(DESC));
    orphan.put(c, m, d, f.id, body(&["w4/Gone"]));
    assert_eq!(
        orphan.invalidate_for_class_change("w4/Gone"),
        1,
        "withdraws, and reaches no manager"
    );
}
