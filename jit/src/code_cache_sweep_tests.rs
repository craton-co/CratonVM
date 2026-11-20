use super::{
    code_cache_sweep_enabled, CodeCacheSweepState, CompiledMethod, ExecutableBuffer, JitCache,
    JitCodeCacheSweepRefusal,
};

/// `(artifact_id, entry count, entry address)`, as `sweep_cold_bodies`
/// builds it from the live set, with the total the caller also computes.
fn census(rows: &[(u64, u64, usize)]) -> (Vec<(u64, u64, usize)>, u64) {
    (rows.to_vec(), rows.iter().map(|&(_, count, _)| count).sum())
}

/// CODECACHE21-6: the withdrawal is keyed on the artifact the plan chose.
/// A row whose address is doomed contributes its artifact id; a body that
/// only shares a doomed address (a recycled buffer) is not in `live`, so
/// its id is never in the set.
#[test]
fn cc21_the_doomed_set_is_keyed_on_artifacts_not_addresses() {
    let live = [(1u64, 0u64, 0x1000usize), (2, 5, 0x2000), (3, 0, 0x3000)];
    let doomed: rustc_hash::FxHashSet<usize> = [0x1000usize, 0x3000].into_iter().collect();
    let ids = super::doomed_artifact_ids(&live, &doomed);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&1) && ids.contains(&3));
    assert!(!ids.contains(&2), "a body that moved is not doomed");
    assert!(
        !ids.contains(&99),
        "a later artifact at a doomed address is not in the plan"
    );
}

/// The first sweep of a cache retires nothing, however cold everything
/// looks. There is only one reading, so "has not moved" is not yet a
/// statement about anything — and a body compiled between the cap being
/// hit and this call has by construction not been entered. Without this
/// arm the very first sweep would withdraw the entire cache.
#[test]
fn the_first_sweep_only_takes_a_census() {
    let mut state = CodeCacheSweepState::default();
    let (live, total) = census(&[(1, 0, 0x1000), (2, 0, 0x2000)]);
    let (doomed, refusal) = state.plan(&live, total, 0);
    assert!(doomed.is_empty());
    assert_eq!(refusal, Some(JitCodeCacheSweepRefusal::FirstSweepIsACensus));
    assert_eq!(state.sweeps, 1, "the census still counts as a sweep");
    assert_eq!(state.seen.len(), 2, "and it still records both readings");
}

/// The rate limit. Two sweeps with NOTHING between them -- no body entered,
/// no progress on the caller's clock -- see identical readings, and the
/// ENTIRE cache would qualify as "has not moved". Removing this arm turns
/// a back-to-back second call into a full flush.
#[test]
fn a_sweep_with_nothing_run_since_the_last_is_refused() {
    let mut state = CodeCacheSweepState::default();
    let (live, total) = census(&[(1, 4, 0x1000), (2, 6, 0x2000)]);
    state.plan(&live, total, 0);
    let (doomed, refusal) = state.plan(&live, total, 0);
    assert!(doomed.is_empty());
    assert_eq!(
        refusal,
        Some(JitCodeCacheSweepRefusal::NothingRanSinceTheLastSweep)
    );
    assert_eq!(state.sweeps, 1, "a refused sweep must not count as one");
}

/// **The RT-8 soak's finding.** The program moved on: the interpreter ran
/// (the clock advanced) and not one compiled body was entered. That is the
/// phase change at the cap -- new code is refused compilation and runs
/// interpreted, old code is dead -- and every body in the cache is cold.
///
/// Before 2026-09-18 the rate limit read "no body entered" as "nothing
/// ran" and refused here, on every sweep, for the life of the process:
/// `tools/probes/SweepPhases` at a 1 MiB cap swept once in 30 s and then
/// never again. Make the refusal ignore the clock and this fails.
#[test]
fn an_idle_cache_is_swept_when_the_process_moved_on_without_it() {
    let mut state = CodeCacheSweepState::default();
    let (live, total) = census(&[(1, 4, 0x1000), (2, 6, 0x2000)]);
    state.plan(&live, total, 100);
    let (doomed, refusal) = state.plan(&live, total, 356);
    assert_eq!(refusal, None, "the process ran; this is not the idle case");
    assert_eq!(
        doomed.len(),
        2,
        "no body was entered, so every body is cold"
    );
}

/// The policy itself: of two bodies, the one whose counter stood still
/// while the other ran is the cold one. This is the whole difference
/// between this policy and "least-invoked" — `0x1000` has been entered
/// FOUR times and `0x2000` only twice, and it is `0x1000` that goes,
/// because a count is a history and a delta is a recency.
#[test]
fn only_bodies_whose_count_did_not_move_are_swept() {
    let mut state = CodeCacheSweepState::default();
    let (first, first_total) = census(&[(1, 4, 0x1000), (2, 2, 0x2000)]);
    state.plan(&first, first_total, 0);
    let (second, second_total) = census(&[(1, 4, 0x1000), (2, 9, 0x2000)]);
    let (doomed, refusal) = state.plan(&second, second_total, 0);
    assert_eq!(refusal, None);
    assert_eq!(doomed.len(), 1);
    assert!(doomed.contains(&0x1000));
    assert!(!doomed.contains(&0x2000));
}

/// A body published since the last sweep has no previous reading, and "no
/// previous reading" must not read as "has not moved". It is the method
/// that just went hot enough to be compiled, so sweeping it is both the
/// most expensive possible mistake and an invisible one — it would simply
/// be recompiled and swept again, forever.
#[test]
fn a_body_first_seen_on_this_sweep_is_never_a_candidate() {
    let mut state = CodeCacheSweepState::default();
    let (first, first_total) = census(&[(1, 5, 0x1000), (3, 1, 0x3000)]);
    state.plan(&first, first_total, 0);
    let (second, second_total) = census(&[(1, 5, 0x1000), (2, 0, 0x2000), (3, 4, 0x3000)]);
    let (doomed, _) = state.plan(&second, second_total, 0);
    assert!(
        !doomed.contains(&0x2000),
        "artifact 2 is new; its count of 0 is not evidence of coldness"
    );
    assert!(doomed.contains(&0x1000), "artifact 1 genuinely stood still");
}

/// The snapshot is keyed on `artifact_id`, not on the entry ADDRESS, and
/// this is the test that says why. An executable buffer is recycled to the
/// next compile of a similar size, so an address-keyed snapshot is an ABA:
/// here a brand-new artifact 9 lands on the address artifact 1 vacated,
/// with the same count artifact 1 had. Keyed on the address it would match
/// and be swept on its first sweep; keyed on the id it is simply unknown.
#[test]
fn the_snapshot_is_keyed_on_artifact_id_not_the_recycled_entry_address() {
    let mut state = CodeCacheSweepState::default();
    let (first, first_total) = census(&[(1, 5, 0x1000), (7, 2, 0x7000)]);
    state.plan(&first, first_total, 0);
    let (second, second_total) = census(&[(9, 5, 0x1000), (7, 3, 0x7000)]);
    let (doomed, _) = state.plan(&second, second_total, 0);
    assert!(
        doomed.is_empty(),
        "the artifact at 0x1000 is a different one from the artifact the \
         previous reading of 0x1000 described"
    );
}

/// The snapshot is rebuilt from the live set, so it cannot grow without
/// bound as artifacts come and go. `artifact_id` is monotonic and never
/// repeats, so an update-in-place map would only ever accumulate.
#[test]
fn the_snapshot_holds_only_bodies_that_are_still_live() {
    let mut state = CodeCacheSweepState::default();
    let (first, first_total) = census(&[(1, 1, 0x1000), (2, 1, 0x2000), (3, 1, 0x3000)]);
    state.plan(&first, first_total, 0);
    assert_eq!(state.seen.len(), 3);
    let (second, second_total) = census(&[(4, 9, 0x4000)]);
    state.plan(&second, second_total, 0);
    assert_eq!(state.seen.len(), 1);
    assert_eq!(state.seen.get(&4), Some(&9));
}

/// Default OFF, and it declines before taking the census — a sweeper that
/// walked every published body to then do nothing would be paying the
/// whole cost of the feature in a configuration that does not have it.
#[test]
fn the_sweeper_is_off_by_default_and_declines_before_looking() {
    if code_cache_sweep_enabled() {
        eprintln!(
            "CRATONVM_JIT_CODE_CACHE_SWEEP is set in this process; this test \
             is about the DEFAULT and has nothing to assert here"
        );
        return;
    }
    let cache = JitCache::new();
    let report = cache.sweep_cold_bodies(1);
    assert_eq!(
        report.refused,
        Some(JitCodeCacheSweepRefusal::Disabled),
        "the sweeper must refuse explicitly, not return an empty report — \
         'disabled' and 'nothing was cold' are different facts"
    );
    assert_eq!(report.observed, 0);
    assert_eq!(
        cache.code_cache_sweeps(),
        0,
        "a disabled sweeper must not even advance the sweep state"
    );
}

/// **RT-8's stuck-at-C1 residual, closed.**
///
/// `invalidate_matching` withdraws the transitive reverse closure over
/// baked direct calls: evict a cold callee and every caller that baked a
/// direct call into it goes too. Those closure members used to be computed
/// inside that function and thrown away, so the sweeper -- which pre-scanned
/// the shard maps for its own cold CANDIDATES -- reported only the
/// candidates. A caller withdrawn this way lost its body and was never
/// handed to `note_body_withdrawn`, and since `current_tier` only advances
/// it could never be offered for compilation again.
///
/// Here `caller` is not cold and does not match the predicate. It is
/// withdrawn anyway, because it baked a direct call into `callee`. The
/// assertion is that it comes BACK -- `withdrawn` must account for `swept`,
/// not for the predicate.
#[test]
fn the_reverse_closure_is_reported_and_not_just_withdrawn() {
    use std::sync::Arc;

    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(1);
    let desc: Arc<str> = Arc::from("()V");
    let class: Arc<str> = Arc::from("ClosureReportTest");

    let mut callee_buf = ExecutableBuffer::new(64).expect("executable buffer");
    callee_buf.emit(&[0xC3]);
    let callee_name: Arc<str> = Arc::from("callee");
    cache.put(
        class.clone(),
        callee_name.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(callee_buf),
    );
    let callee_entry = cache
        .get(&class, &callee_name, &desc, cid)
        .expect("the callee is published")
        .entry_ptr() as usize;

    // The caller bakes a direct call into the callee -- the edge the
    // closure walks.
    let mut caller_buf = ExecutableBuffer::new(64).expect("executable buffer");
    caller_buf.emit(&[0xC3]);
    let mut caller = CompiledMethod::new(caller_buf);
    caller._direct_callee_entries = vec![callee_entry];
    let caller_name: Arc<str> = Arc::from("caller");
    cache.put(
        class.clone(),
        caller_name.clone(),
        desc.clone(),
        cid,
        caller,
    );

    // A third body with no edge to either, so "everything was withdrawn"
    // cannot pass for "the closure was followed".
    let mut other_buf = ExecutableBuffer::new(64).expect("executable buffer");
    other_buf.emit(&[0xC3]);
    let other_name: Arc<str> = Arc::from("unrelated");
    cache.put(
        class.clone(),
        other_name.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(other_buf),
    );

    // The predicate names ONLY the callee -- exactly what a cold-candidate
    // set looks like to `invalidate_matching`.
    // Read BEFORE the withdrawal: a buffer's capacity is its mapping,
    // rounded up by the allocator, so it is not simply the 64 asked for.
    let capacity_of = |name: &Arc<str>| {
        cache
            .get(&class, name, &desc, cid)
            .expect("published")
            ._buffer
            .capacity() as u64
    };
    let expected_bytes = capacity_of(&callee_name) + capacity_of(&caller_name);

    let (swept, withdrawn, withdrawn_bytes) = cache
        .invalidate_matching_collecting(crate::retire_reason::INVALIDATED, |key, _| {
            &*key.method_name == "callee"
        });
    // Exactly the callee's and the closure member's mappings: a total that
    // omitted the closure would under-count, and one that included
    // `unrelated` would count a body that is still live.
    assert_eq!(
        withdrawn_bytes, expected_bytes,
        "the byte total must be the callee's plus the caller's capacity"
    );

    assert_eq!(
        swept, 2,
        "the callee and the caller that baked a direct call into it",
    );

    let names: Vec<&str> = withdrawn.iter().map(|(_, m, _, _)| &**m).collect();
    assert!(
        names.contains(&"callee"),
        "the candidate itself must be reported; got {names:?}",
    );
    assert!(
        names.contains(&"caller"),
        "THE POINT: the closure member must be reported too, or it loses \
         its body without being demoted and is stuck at its old tier \
         forever. Got {names:?}",
    );
    assert!(
        !names.contains(&"unrelated"),
        "a body with no edge to the candidate must survive; got {names:?}",
    );
    assert_eq!(
        withdrawn.len(),
        swept,
        "`withdrawn` must account for `swept`, which is the property the \
         sweeper's demotion loop depends on",
    );
}

/// The empty case allocates nothing and reports nothing.
///
/// This runs on every class define, and the fast path returning
/// `(0, Vec::new())` is what keeps it free.
#[test]
fn a_withdrawal_that_matches_nothing_reports_nothing() {
    use std::sync::Arc;

    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(1);
    let desc: Arc<str> = Arc::from("()V");
    let class: Arc<str> = Arc::from("NoMatchTest");
    let name: Arc<str> = Arc::from("m");
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        name.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );

    let (swept, withdrawn, withdrawn_bytes) = cache
        .invalidate_matching_collecting(crate::retire_reason::INVALIDATED, |key, _| {
            &*key.method_name == "absent"
        });
    assert_eq!(swept, 0);
    assert_eq!(withdrawn_bytes, 0, "nothing withdrawn, nothing counted");
    assert!(withdrawn.is_empty());
    assert_eq!(
        withdrawn.capacity(),
        0,
        "the empty path must not allocate -- it is on every class define",
    );
}
