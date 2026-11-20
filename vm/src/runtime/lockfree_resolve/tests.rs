use super::*;
use std::sync::{Arc, Mutex};

static RESOLVE_CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());
use std::thread;

// -- T10.4 promoted invoke cache --------------------------------------

fn sample_bytecode_method(
    class_id: u32,
) -> std::sync::Arc<cratonvm_jit_api::CachedBytecodeMethod> {
    std::sync::Arc::new(cratonvm_jit_api::CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(class_id),
            class_name: std::sync::Arc::from("A"),
            method_name: std::sync::Arc::from("m"),
            method_descriptor: std::sync::Arc::from("()V"),
            source_file: None,
            code: std::sync::Arc::from(vec![0xB1u8].as_slice()),
            exception_table: std::sync::Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        },
    ))
}

#[test]
fn t10_shared_resolution_read_hit_bypasses_write_lock() {
    let state = SharedResolutionState::new();
    let key: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(20)));
    let target = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id: ClassId::new(20),
        cached: sample_bytecode_method(20),
        gate: crate::classloading::resolution::RedefineGate::never_stale(),
    };
    // Miss first — returns None, does not bump hit counter.
    assert!(state.get_promoted_invoke(&key).is_none());
    assert_eq!(state.promoted_hit_count(), 0);
    // Insert once (takes the only write-lock for this call site).
    state.insert_promoted_invoke(key, target.clone());
    assert_eq!(state.promoted_insert_count(), 1);
    // Hit — advances the hit counter, returns an equivalent target,
    // and takes only a read-lock internally.
    let got = state.get_promoted_invoke(&key).expect("hit expected");
    match got {
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id, ..
        } => {
            assert_eq!(receiver_class_id, ClassId::new(20));
        }
        _ => panic!("wrong variant cached"),
    }
    assert_eq!(state.promoted_hit_count(), 1);
    assert_eq!(state.promoted_invoke_count(), 1);
}

#[test]
fn t10_shared_resolution_distinguishes_receiver_classes() {
    // Same call site, two different receiver classes must not collide.
    let state = SharedResolutionState::new();
    let k1: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(20)));
    let k2: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(21)));
    let t1 = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id: ClassId::new(20),
        cached: sample_bytecode_method(20),
        gate: crate::classloading::resolution::RedefineGate::never_stale(),
    };
    let t2 = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id: ClassId::new(21),
        cached: sample_bytecode_method(21),
        gate: crate::classloading::resolution::RedefineGate::never_stale(),
    };
    state.insert_promoted_invoke(k1, t1);
    state.insert_promoted_invoke(k2, t2);
    assert_eq!(state.promoted_invoke_count(), 2);
    match state.get_promoted_invoke(&k1).unwrap() {
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id, ..
        } => {
            assert_eq!(receiver_class_id, ClassId::new(20));
        }
        _ => panic!("wrong variant"),
    }
    match state.get_promoted_invoke(&k2).unwrap() {
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id, ..
        } => {
            assert_eq!(receiver_class_id, ClassId::new(21));
        }
        _ => panic!("wrong variant"),
    }
}

/// A redefinition of the class an entry is bound to makes that entry stale,
/// and the next read evicts it rather than serving it.
///
/// This replaces `t10_shared_resolution_invalidate_for_class_drops_both_ends`,
/// which drove `invalidate_promoted_for_class` — a function no production
/// path ever called, deleted above. The retired
/// `promoted-invoke-resolutions-survive-class-redefinition-20260903`
/// write-up inferred a stale-dispatch window from that absence and said, to
/// its credit, that nobody had looked at `get_promoted_invoke`'s consumers
/// yet. This is what looking finds: the eviction is per-entry, on use, and
/// immediate.
///
/// The assertion that earns its place is the second one. Returning `None`
/// alone would leave the stale entry for every sibling thread to re-discover
/// and re-evict; the read path removes it, so the count goes to zero.
#[test]
fn a_redefine_generation_bump_evicts_the_promoted_entry_on_the_next_read() {
    let state = SharedResolutionState::new();
    // The handle `ClassManager::class_redefine_generation_handle` hands to
    // the populate path for the DECLARING class, and that `redefine_class`
    // bumps with `Release` in its step 7.
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let key: PromotedInvokeKey = (ClassId::new(10), 7, false, Some(ClassId::new(30)));
    state.insert_promoted_invoke(
        key,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(30),
            cached: sample_bytecode_method(30),
            gate: crate::classloading::resolution::RedefineGate::snapshot(
                std::sync::Arc::clone(&counter),
            ),
        },
    );
    assert!(
        state.get_promoted_invoke(&key).is_some(),
        "a fresh entry is served"
    );
    assert_eq!(state.promoted_invoke_count(), 1);

    // `class_manager::redefine_class` step 7, in one line.
    counter.fetch_add(1, std::sync::atomic::Ordering::Release);

    assert!(
        state.get_promoted_invoke(&key).is_none(),
        "the pre-redefinition target must not be served"
    );
    assert_eq!(
        state.promoted_invoke_count(),
        0,
        "and the stale entry is evicted on detection, not left until the next GC"
    );
}

/// Wholesale clear empties the live cache.
///
/// Was `t10_invalidate_all_clears_promoted_cache`, driving the since-deleted
/// `invalidate_all` (ARCH-2026-08-04 A7). `invalidate_promoted` is the same
/// operation on the only map this type ever owned, so the assertions are
/// unchanged — only the entry point moved.
#[test]
fn t10_invalidate_promoted_clears_promoted_cache() {
    let state = SharedResolutionState::new();
    let key: PromotedInvokeKey = (ClassId::new(1), 0, false, None);
    state.insert_promoted_invoke(
        key,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(2),
            cached: sample_bytecode_method(2),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        },
    );
    assert_eq!(state.promoted_invoke_count(), 1);
    state.invalidate_promoted();
    assert_eq!(state.promoted_invoke_count(), 0);
    assert!(state.get_promoted_invoke(&key).is_none());
}

#[test]
fn t10_shared_resolution_concurrent_reads_do_not_serialize() {
    // Ensure the read path uses a read-lock: multiple reader threads
    // should all observe the same hit without serialising.  We detect
    // serialisation indirectly — all N*M reads must complete and the
    // hit counter must end at exactly N*M.
    let state = std::sync::Arc::new(SharedResolutionState::new());
    let key: PromotedInvokeKey = (ClassId::new(5), 3, false, None);
    state.insert_promoted_invoke(
        key,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(7),
            cached: sample_bytecode_method(7),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        },
    );
    let mut handles = Vec::new();
    for _ in 0..4 {
        let s = std::sync::Arc::clone(&state);
        handles.push(std::thread::spawn(move || {
            for _ in 0..50 {
                assert!(s.get_promoted_invoke(&key).is_some());
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(state.promoted_hit_count(), 4 * 50);
    // Only one insert (the setup) — no write contention occurred.
    assert_eq!(state.promoted_insert_count(), 1);
}

// -- Resolution flow integration test ---------------------------------

// -- HIGH security harden tests ---------------------------------------
//
// These tests prove the cache rejects (1) forged hash collisions
// (an adversary crafts a triple whose `FxHasher` digests match a
// cached entry's despite the strings differing — would return the
// wrong target on the vulnerable key) and (2) cross-classloader
// contamination (loader L1 and L2 both load the same FQN — JVMS
// §5.3.4 makes the defining loader part of class identity).

// -- HIGH security: shared-cache capacity bound -----------------------
//
// The cross-thread shared maps were previously unbounded — a workload
// minting unbounded distinct keys could exhaust memory (DoS). These
// tests prove the cap holds and that an evicted entry simply misses
// (re-resolution is correctness-preserving), never panics.

#[test]
fn evict_to_fit_keeps_map_under_cap() {
    // Direct, env-independent test of the eviction core. Insert far more
    // than `cap` entries, evicting before each insert, and assert the map
    // never exceeds `cap`.
    const CAP: usize = 8;
    let mut map: FxHashMap<u64, u64> = fx_hashmap();
    for i in 0..1000u64 {
        evict_to_fit(&mut map, CAP);
        map.insert(i, i);
        assert!(
            map.len() <= CAP,
            "map exceeded cap: len={} cap={CAP}",
            map.len()
        );
    }
    // After the run the map is full but bounded.
    assert_eq!(map.len(), CAP);
}

#[test]
fn evict_to_fit_zero_cap_does_not_spin_on_empty() {
    // Defensive: a `cap == 0` against an empty map must terminate (the
    // emptiness guard breaks the loop) rather than spin forever.
    let mut map: FxHashMap<u64, u64> = fx_hashmap();
    evict_to_fit(&mut map, 0);
    assert!(map.is_empty());
}

/// RAII helper for the test-only capacity override, so a failing
/// assertion cannot leak a forced cap into the rest of the suite.
struct CapOverride;
impl CapOverride {
    fn set(v: usize) -> Self {
        tests_cap_override::set(v);
        CapOverride
    }
}
impl Drop for CapOverride {
    fn drop(&mut self) {
        tests_cap_override::set(0);
    }
}

#[test]
fn shared_cache_cap_parses_env_override() {
    assert_eq!(parse_shared_cache_cap(Some("10")), 10);
    assert_eq!(parse_shared_cache_cap(Some("0")), DEFAULT_SHARED_CACHE_CAP);
    assert_eq!(parse_shared_cache_cap(Some("")), DEFAULT_SHARED_CACHE_CAP);
    assert_eq!(
        parse_shared_cache_cap(Some("not-a-number")),
        DEFAULT_SHARED_CACHE_CAP
    );
    assert_eq!(parse_shared_cache_cap(None), DEFAULT_SHARED_CACHE_CAP);
}

#[test]
fn shared_cache_cap_is_memoized_for_process_lifetime() {
    // The public path is a stable process-lifetime value.
    let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
    let first = shared_cache_cap();
    assert_eq!(
        shared_cache_cap(),
        first,
        "shared_cache_cap must not re-read the environment"
    );
}

// -- 2026-07-26 arch pass: wiring assertions -------------------------

#[test]
fn promoted_invoke_round_trip_is_the_only_live_path() {
    // Guards the module doc's claim about what is wired: the
    // promoted-invoke map is the only tier, and its read path must not
    // require the class manager. The name is kept from when this file also
    // held three dead tiers (ARCH-2026-08-04 A7) — it is now a statement
    // about the whole module, not a contrast with its neighbours.
    let state = SharedResolutionState::new();
    let key: PromotedInvokeKey = (ClassId::new(1), 4, false, Some(ClassId::new(2)));
    assert!(state.get_promoted_invoke(&key).is_none());
    assert_eq!(state.promoted_hit_count(), 0);

    state.insert_promoted_invoke(
        key,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(2),
            cached: sample_bytecode_method(2),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        },
    );
    assert!(state.get_promoted_invoke(&key).is_some());
    assert_eq!(state.promoted_hit_count(), 1);
    assert_eq!(state.promoted_insert_count(), 1);

    // A miss must not be memoized as a negative anywhere: after
    // invalidation the same key simply re-misses and can be re-promoted.
    // (Was `invalidate_promoted_for_class(ClassId::new(1))`; the wholesale
    // clear is the same operation on a map holding one key, and it is the
    // only invalidation entry point this type still has.)
    state.invalidate_promoted();
    assert!(state.get_promoted_invoke(&key).is_none());
    state.insert_promoted_invoke(
        key,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(2),
            cached: sample_bytecode_method(2),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        },
    );
    assert!(state.get_promoted_invoke(&key).is_some());
}

/// A wholesale clear leaves no memoized negatives behind.
///
/// Was `invalidate_promoted_matches_invalidate_all_for_the_live_cache`,
/// which compared `invalidate_promoted` against the since-deleted
/// `invalidate_all` (ARCH-2026-08-04 A7). That comparison is meaningless now
/// that only one map exists, but the second half of the test is not: it
/// covers what `promoted_invoke_round_trip_is_the_only_live_path` does not,
/// namely that a key cleared by a *wholesale* sweep (`gc.rs`'s loader-unload
/// path, CR-LR-1) re-misses and can be re-promoted, rather than sticking as
/// a negative entry.
#[test]
fn wholesale_clear_leaves_no_memoized_negative() {
    let make = || {
        let state = SharedResolutionState::new();
        for cp in 0..4u16 {
            let key: PromotedInvokeKey = (ClassId::new(1), cp, false, Some(ClassId::new(2)));
            state.insert_promoted_invoke(
                key,
                CachedInvokeTarget::VirtualBytecode {
                    receiver_class_id: ClassId::new(2),
                    cached: sample_bytecode_method(2),
                    gate: crate::classloading::resolution::RedefineGate::never_stale(),
                },
            );
        }
        state
    };

    let via_promoted = make();
    assert_eq!(via_promoted.promoted_invoke_count(), 4);

    via_promoted.invalidate_promoted();
    assert_eq!(via_promoted.promoted_invoke_count(), 0);

    // A cleared key is a miss, not a memoized negative.
    let key: PromotedInvokeKey = (ClassId::new(1), 0, false, Some(ClassId::new(2)));
    assert!(via_promoted.get_promoted_invoke(&key).is_none());
    via_promoted.insert_promoted_invoke(
        key,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(2),
            cached: sample_bytecode_method(2),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        },
    );
    assert!(via_promoted.get_promoted_invoke(&key).is_some());
}

// -- ARCH-2026-08-04 A8: sharding ------------------------------------

/// Call sites of one caller class must spread across shards.
///
/// This is the whole point of hashing the full key instead of just
/// `caller_class`. Sharding on the caller alone would put every call site
/// of one hot class — exactly the class being consulted in a tight loop —
/// on a single shard, reproducing the contention A8 removed under a new
/// name. A pre-A8 single map is the degenerate case of this test with one
/// bucket, so it would fail here.
#[test]
fn one_caller_class_spreads_across_shards() {
    let mut seen = std::collections::HashSet::new();
    for cp in 0..256u16 {
        let key: PromotedInvokeKey = (ClassId::new(1), cp, false, Some(ClassId::new(2)));
        seen.insert(shard_of(&key));
    }
    assert_eq!(
        seen.len(),
        PROMOTED_SHARDS,
        "256 call sites of one caller landed on {} of {} shards — the shard \
         function is not spreading a hot class's sites",
        seen.len(),
        PROMOTED_SHARDS
    );
}

/// The shard of a key never moves.
///
/// `get_promoted_invoke` and `insert_promoted_invoke` compute the shard
/// independently; if `shard_of` were not a pure function of the key, an
/// insert and its lookup could land on different shards and every promotion
/// would silently miss — a pure slowdown with no visible symptom.
#[test]
fn shard_of_is_stable_for_a_key() {
    let key: PromotedInvokeKey = (ClassId::new(7), 42, true, Some(ClassId::new(9)));
    let first = shard_of(&key);
    for _ in 0..1000 {
        assert_eq!(shard_of(&key), first);
    }
    assert!(first < PROMOTED_SHARDS);
}

/// The DoS bound survives sharding.
///
/// `shared_cache_cap` exists so a workload minting unbounded distinct keys
/// (a class emitting fresh lambda / proxy / hidden-class names per call)
/// cannot grow the cache until the process dies. Splitting one capped map
/// into 16 uncapped ones would have quietly deleted that property, so this
/// mints far more keys than the cap allows and checks the total.
#[test]
fn the_shared_cap_still_bounds_the_total_across_shards() {
    let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
    let _override = CapOverride::set(64);

    let state = SharedResolutionState::new();
    for cp in 0..4096u16 {
        state.insert_promoted_invoke(
            (ClassId::new(1), cp, false, Some(ClassId::new(2))),
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: ClassId::new(2),
                cached: sample_bytecode_method(2),
                gate: crate::classloading::resolution::RedefineGate::never_stale(),
            },
        );
    }

    // Per-shard cap is ceil(64/16) = 4, so the aggregate ceiling is 64.
    let total = state.promoted_invoke_count();
    assert!(
        total <= per_shard_cap() * PROMOTED_SHARDS,
        "4096 minted keys grew the cache to {total}, above the \
         {} the per-shard cap allows",
        per_shard_cap() * PROMOTED_SHARDS
    );
    assert!(
        total > 0,
        "the cap evicted everything — a freshly-inserted entry must survive"
    );
}

/// A cap of 1 must not evict the entry it was making room for.
///
/// `parse_shared_cache_cap` enforces a floor of 1 precisely so a fresh
/// insert survives. Dividing that by `PROMOTED_SHARDS` without the `max(1)`
/// in `per_shard_cap` yields 0, and `evict_to_fit` with `cap == 0` evicts
/// unconditionally — the cache would then hold nothing at all, turning a
/// tuning knob into a silent total disable.
#[test]
fn a_cap_of_one_still_admits_an_entry() {
    let _guard = RESOLVE_CACHE_ENV_LOCK.lock().unwrap();
    let _override = CapOverride::set(1);

    assert_eq!(per_shard_cap(), 1, "per-shard cap must never round to 0");

    let state = SharedResolutionState::new();
    let key: PromotedInvokeKey = (ClassId::new(1), 0, false, Some(ClassId::new(2)));
    state.insert_promoted_invoke(
        key,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(2),
            cached: sample_bytecode_method(2),
            gate: crate::classloading::resolution::RedefineGate::never_stale(),
        },
    );
    assert!(
        state.get_promoted_invoke(&key).is_some(),
        "a cap of 1 must still admit one entry per shard"
    );
}

/// Each shard occupies its own cache line.
///
/// The padding is what makes sharding worth anything: without it every
/// shard's lock word and counters share one or two lines and the cores
/// ping-pong exactly as they did with a single lock. A future edit that
/// drops `repr(align(64))` — or adds a field pushing the shard over a line
/// — would leave a correct cache that scales no better than the one A8
/// replaced, with nothing to show for it.
#[test]
fn shards_do_not_share_a_cache_line() {
    assert_eq!(
        std::mem::align_of::<PromotedShard>(),
        64,
        "PromotedShard must be cache-line aligned"
    );
    assert_eq!(
        std::mem::size_of::<PromotedShard>() % 64,
        0,
        "PromotedShard must be a whole number of cache lines so shard N+1 \
         does not start inside shard N's line"
    );
}

/// Interpreter round i1 wave 18, lane L4: the CALLER side of a promoted
/// entry. A redefinition replaces the caller's constant pool under the
/// same id, so an entry resolved before any redefinition may name the old
/// pool's member at its key although its target's gate is unchanged. A
/// promotion whose resolution straddled a redefinition is not published,
/// and an entry stamped with an earlier count is evicted on its next read.
/// The process count is not moved here (other tests fill caches in this
/// process); the stamps are set around it instead.
#[test]
fn a_promotion_resolved_before_a_redefinition_is_neither_published_nor_served() {
    if !promoted_retire_redefined_callers() {
        return;
    }
    let state = SharedResolutionState::new();
    let key: PromotedInvokeKey = (ClassId::new(18_421), 5, false, None);
    let target = || CachedInvokeTarget::Bytecode {
        cached: sample_bytecode_method(18_422),
        gate: crate::classloading::resolution::RedefineGate::never_stale(),
    };
    let now = redefinitions_now();
    state.insert_promoted_invoke_as_of(key, target(), now.wrapping_sub(1));
    assert_eq!(
        state.promoted_invoke_count(),
        0,
        "a resolution that began before the last redefinition is not published"
    );
    // An entry stamped with a count the redefinition ring cannot vouch
    // for (a span longer than the ring, or one it has lapped): since wave
    // 20 an entry is kept across OTHER classes' redefinitions only when
    // the ring names every one of them.
    state.shards[shard_of(&key)]
        .map
        .write()
        .insert(key, (now.wrapping_sub(1 << 20), target()));
    assert!(state.get_promoted_invoke(&key).is_none());
    assert_eq!(state.promoted_invoke_count(), 0, "evicted on detection");
    // A promotion under the current count is served.
    state.insert_promoted_invoke_as_of(key, target(), redefinitions_now());
    if redefinitions_now() == now {
        assert!(state.get_promoted_invoke(&key).is_some());
    }
}
/// nio21 wave 5 L1: a state bound to its VM's redefinition log keeps an
/// entry, and admits a promotion, across process-clock moves its own VM
/// made none of (here: a fresh log, so every move is another VM's or
/// another test's). The unbound arm is the test above.
#[test]
fn r21w5_a_bound_state_judges_entries_by_its_own_vm() {
    if !promoted_retire_redefined_callers() {
        return;
    }
    let log = cratonvm_classloading::RedefinitionLog::new_shared();
    let state = SharedResolutionState::with_redefinition_log(log);
    if !state.has_redefinition_log() {
        // `CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS=0` in the environment.
        return;
    }
    let key: PromotedInvokeKey = (ClassId::new(21_521), 5, false, None);
    let target = || CachedInvokeTarget::Bytecode {
        cached: sample_bytecode_method(21_522),
        gate: crate::classloading::resolution::RedefineGate::never_stale(),
    };
    // Stamped at count 0: every later move of the process clock was some
    // other VM's, and this VM's log has no record.
    state.insert_promoted_invoke_as_of(key, target(), 0);
    assert_eq!(
        state.promoted_invoke_count(),
        1,
        "a foreign straddle is admitted"
    );
    assert!(state.get_promoted_invoke(&key).is_some(), "and served");
    // A backwards span is never vouched for.
    assert!(state.promotion_straddles_a_redefinition(5, 4));
    assert!(!state.promotion_straddles_a_redefinition(7, 7));
}
