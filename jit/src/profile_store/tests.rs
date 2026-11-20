use super::*;
use std::sync::Arc;

// ── fixtures ────────────────────────────────────────────────────────────

/// A five-class name space. [`namer`] and [`resolver`] are exact inverses
/// over it — which is what a real VM's class manager gives us, and what the
/// identity-by-name property is stated against.
const CLASSES: &[&str] = &[
    "java/lang/Object",
    "java/util/HashMap",
    "org/example/Service",
    "org/example/Impl",
    "org/example/Other",
];

/// Class name -> id, with `base` as the id of `CLASSES[0]`. Two different
/// `base` values model two different runs of the same program.
fn namer(base: u32) -> impl Fn(u32) -> Option<String> {
    move |id: u32| {
        let index = id.checked_sub(base)? as usize;
        CLASSES.get(index).map(|s| (*s).to_string())
    }
}

fn resolver(base: u32) -> impl Fn(&str) -> Option<u32> {
    move |name: &str| {
        CLASSES
            .iter()
            .position(|c| *c == name)
            .map(|i| base + i as u32)
    }
}

/// The live-bytecode resolver the tests seed against: `handle` is 64 bytes
/// and `run` 8, the lengths [`populated`] stamps; every other method is 256
/// bytes, past every pc the tests record.
fn live_len(_class_id: u32, name: &str, _descriptor: &str) -> LiveCode {
    match name {
        "handle" => LiveCode::Len(64),
        "run" => LiveCode::Len(8),
        _ => LiveCode::Len(256),
    }
}

fn key(class_id: u32, name: &str, descriptor: &str) -> MethodKey {
    MethodKey {
        class_id,
        method_name: Arc::from(name),
        descriptor: Arc::from(descriptor),
    }
}

/// A store with every map populated, built at class-id base `base`.
fn populated(base: u32) -> ProfileStore {
    let store = ProfileStore::new();
    let service = key(base + 2, "handle", "(Ljava/lang/Object;)V");
    store.with_method_profile_mut(&service, |p| {
        p.note_code_len(64);
        p.record_branch(7, true);
        p.record_branch(7, true);
        p.record_branch(7, false);
        p.record_branch(19, false);
        p.record_call_site(23);
        p.record_call_site(23);
        p.record_backedge(31);
        p.record_trip_complete(31, 12);
        // Receivers: `Impl` dominant, `Other` in the tail.
        for _ in 0..40 {
            p.record_receiver(11, base + 3);
        }
        for _ in 0..3 {
            p.record_receiver(11, base + 4);
        }
        p.record_receiver(45, base + 1);
    });
    store.add_invocations(
        cratonvm_jit_api::invoc_key_parts(base + 2, "handle", "(Ljava/lang/Object;)V"),
        888,
    );

    let helper = key(base + 3, "run", "()I");
    store.with_method_profile_mut(&helper, |p| {
        p.note_code_len(8);
        p.record_branch(2, false);
        p.record_call_site(5);
    });
    store
}

/// The store's content as the file describes it, so two stores can be
/// compared without depending on `class_id` at all.
fn view(store: &ProfileStore, base: u32) -> Vec<MethodRecord> {
    let mut census = SaveCensus::default();
    collect_records(store, &namer(base), &no_live_code, &mut census, true)
}

/// `view` with the invocation counts blanked. The round trip deliberately
/// does NOT preserve invocation magnitude — `ReplaySeedPolicy` divides and
/// caps it — so the maps are compared here and the counter separately.
fn view_without_invocations(store: &ProfileStore, base: u32) -> Vec<MethodRecord> {
    let mut records = view(store, base);
    for r in &mut records {
        r.invocations = 0;
    }
    records
}

/// A well-formed file from [`populated`]: two methods, between them at
/// least one entry in every array the format has, so the prefix walk in
/// [`every_truncation_is_an_error`] exercises every count.
fn one_of_everything() -> Vec<u8> {
    let store = populated(100);
    let (bytes, _) = serialize(&store, &namer(100));
    bytes
}

// ── round trip ──────────────────────────────────────────────────────────

/// A populated store serialises and deserialises to an equal store.
///
/// Equality is taken on the *file view* rather than on the in-memory store,
/// because the in-memory store is keyed by `class_id` and the whole point
/// of the format is that `class_id` does not survive a process. Two stores
/// that produce the same bytes hold the same profile.
#[test]
fn a_populated_store_round_trips() {
    let source = populated(100);
    let (bytes, save) = serialize(&source, &namer(100));
    assert_eq!(save.methods_written, 2);
    assert!(save.receiver_types_written >= 3);

    let restored = ProfileStore::new();
    let census = match load_into_with_policy(
        &restored,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    ) {
        Ok(c) => c,
        Err(e) => {
            assert!(false, "round trip failed to load: {e}");
            return;
        }
    };
    assert_eq!(census.methods_in_file, 2);
    assert_eq!(census.methods_seeded, 2);
    assert_eq!(census.methods_unresolved_class, 0);

    assert_eq!(
        view(&source, 100),
        view(&restored, 100),
        "an identity-policy round trip must reproduce the store exactly",
    );

    // And the bytes are stable: a second save of the SAME store is
    // byte-identical, which is the determinism claim in the module doc.
    let (again, _) = serialize(&source, &namer(100));
    assert_eq!(bytes, again, "serialisation must be deterministic");

    // Round 11 wave 2 (r11-tier-profile-replay-residuals item 3): the
    // restored store's receivers are all replayed seeds no live run has
    // judged, so saving it writes none of them back. They used to be
    // saved at full strength and re-seeded on every load, never ageing out.
    let (resaved, census) = serialize(&restored, &namer(100));
    assert_eq!(
        census.receiver_types_written, 0,
        "unjudged seeds are not evidence"
    );
    assert_ne!(bytes, resaved);
}

/// Round 11 wave 18 (r11-tier-profile-replay-residuals item 3, the
/// invocation half): a save writes the invocations THIS run earned, not
/// the replay's credit, so the credit of a method the run never called
/// ages out instead of being re-seeded on every load.
#[test]
fn a_save_writes_earned_invocations_not_the_replayed_credit() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let restored = ProfileStore::new();
    if let Err(e) = load_into_with_policy(
        &restored,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    ) {
        assert!(false, "load failed: {e}");
        return;
    }
    let packed = cratonvm_jit_api::invoc_key_parts(102, "handle", "(Ljava/lang/Object;)V");
    assert_eq!(
        restored.add_invocations(packed, 0),
        888,
        "the identity policy credits the whole count"
    );
    let saved_invocations = |bytes: &[u8]| -> Option<u32> {
        decode(bytes)
            .ok()?
            .into_iter()
            .find(|r| r.method_name == "handle")
            .map(|r| r.invocations)
    };
    let (resaved, _) = serialize(&restored, &namer(100));
    // Since round 11 wave 19 the record's replayed branches, call sites and
    // loops are subtracted too, so a method this run never ran has no
    // evidence left and no record at all.
    assert_eq!(
        saved_invocations(&resaved),
        None,
        "no call this run: nothing is written back"
    );
    // Five calls this run land on the same counter as the credit.
    restored.add_invocations(packed, 5);
    let (after_calls, _) = serialize(&restored, &namer(100));
    assert_eq!(
        saved_invocations(&after_calls),
        Some(5),
        "only what was earned"
    );
    // The tests' view keeps the credit, as it keeps unjudged receivers.
    let viewed = view(&restored, 100)
        .into_iter()
        .find(|r| r.method_name == "handle")
        .map(|r| r.invocations);
    assert_eq!(viewed, Some(893));
}

/// Round 11 wave 19 (r11-tier-profile-replay-residuals item 3, the branch /
/// call-site / loop half): a save writes this run's observations at those
/// sites, not the replayed seed under them, so a replayed hint ages out
/// instead of being written back and re-seeded on every load. The seed
/// still serves this run (the in-memory profile keeps it).
#[test]
fn a_save_writes_observed_branches_call_sites_and_loops_not_the_seeds() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let restored = ProfileStore::new();
    if let Err(e) = load_into_with_policy(
        &restored,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    ) {
        assert!(false, "load failed: {e}");
        return;
    }
    let handle = key(102, "handle", "(Ljava/lang/Object;)V");
    // The seeds serve this run: `populated` recorded 7 as 2 taken / 1 not.
    let seeded = restored
        .get_profile(&handle)
        .and_then(|p| p.branches.get(&7).cloned())
        .map(|b| (b.taken, b.not_taken));
    assert_eq!(seeded, Some((2, 1)), "the seed is in the live profile");

    // This run executes each kind of site once.
    restored.with_method_profile_mut(&handle, |p| {
        p.record_branch(7, false);
        p.record_call_site(23);
        p.record_backedge(31);
    });
    let (resaved, _) = serialize(&restored, &namer(100));
    let records = decode(&resaved).unwrap_or_default();
    assert_eq!(
        records
            .iter()
            .map(|r| r.method_name.as_str())
            .collect::<Vec<_>>(),
        vec!["handle"],
        "`run` observed nothing this run and is not written"
    );
    let handle_record = &records[0];
    assert_eq!(
        handle_record.branches,
        vec![(7, 0, 1)],
        "only the live not-taken"
    );
    assert_eq!(handle_record.call_sites, vec![(23, 1)]);
    assert_eq!(handle_record.loops, vec![(31, 1, 0, 0)]);

    // The tests' view still sees seed plus observation.
    let viewed = view(&restored, 100)
        .into_iter()
        .find(|r| r.method_name == "handle")
        .map(|r| (r.branches, r.call_sites));
    assert_eq!(viewed, Some((vec![(7, 2, 2), (19, 0, 1)], vec![(23, 3)])));
}

/// Round 11 wave 2 (r11-tier-profile-replay-residuals item 1): a repeated
/// key (one class name defined by two loaders) is written once, and a file
/// that repeats a key anyway seeds it once.
#[test]
fn a_repeated_method_key_is_written_and_seeded_once() {
    let store = ProfileStore::new();
    // Two loaders' copies of one class: ids 102 and 202 both name
    // `CLASSES[2]` under the namer below.
    let twin_namer = |id: u32| CLASSES.get((id % 100) as usize).map(|s| s.to_string());
    for (id, calls) in [(102u32, 5u32), (202, 9)] {
        store.with_method_profile_mut(&key(id, "handle", "()V"), |p| {
            p.record_branch(3, true);
        });
        store.add_invocations(
            cratonvm_jit_api::invoc_key_parts(id, "handle", "()V"),
            calls,
        );
    }
    let mut census = SaveCensus::default();
    let records = collect(&store, &twin_namer, &mut census);
    assert_eq!(records.len(), 1, "one record per name");
    assert_eq!(
        records[0].invocations, 9,
        "the hottest copy is the one kept"
    );

    // A hand-made file carrying the record twice seeds it once.
    let doubled = encode(&[records[0].clone(), records[0].clone()]);
    let decoded = match decode(&doubled) {
        Ok(d) => d,
        Err(e) => {
            assert!(false, "a repeated key is dropped, not refused: {e}");
            return;
        }
    };
    assert_eq!(decoded.len(), 1);

    // A repeated pc inside a record is refused.
    let mut bad = records[0].clone();
    bad.branches.push(bad.branches[0]);
    assert!(decode(&encode(&[bad])).is_err());
}

/// Round 12 wave 1: two loaders' classes of one NAME at one receiver site
/// are written as one entry (their total), and a file that repeats a name
/// at a site anyway seeds that class once, within `receiver_ceiling`.
#[test]
fn a_receiver_name_repeated_at_a_site_is_written_and_seeded_once() {
    let store = ProfileStore::new();
    // Ids 103 and 203 both name `CLASSES[3]` (`org/example/Impl`).
    let twin_namer = |id: u32| CLASSES.get((id % 100) as usize).map(|s| s.to_string());
    store.with_method_profile_mut(&key(102, "handle", "()V"), |p| {
        for _ in 0..5 {
            p.record_receiver(11, 103);
        }
        for _ in 0..9 {
            p.record_receiver(11, 203);
        }
    });
    let mut census = SaveCensus::default();
    let records = collect(&store, &twin_namer, &mut census);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].receivers,
        vec![(11u32, vec![(CLASSES[3].to_string(), 14u32)])],
        "one entry per name, carrying the name's total"
    );

    // A hand-made file that names the class three times at the site.
    let mut repeated = records[0].clone();
    repeated.receivers = vec![(
        11,
        vec![
            (CLASSES[3].to_string(), 30),
            (CLASSES[3].to_string(), 30),
            (CLASSES[3].to_string(), 30),
        ],
    )];
    let policy = ReplaySeedPolicy {
        receiver_ceiling: 20,
        ..ReplaySeedPolicy::identity()
    };
    let restored = ProfileStore::new();
    let loaded = load_into_with_policy(
        &restored,
        &encode(&[repeated]),
        &resolver(100),
        &live_len,
        &policy,
    );
    assert!(loaded.is_ok(), "a repeated name is merged, not refused");
    let seeded = restored
        .get_profile(&key(102, "handle", "()V"))
        .and_then(|p| p.receivers.get(&11).and_then(|s| s.get(&103).copied()));
    assert_eq!(
        seeded,
        Some(20),
        "seeded once, at the ceiling, not three times"
    );
}

/// Identity is by NAME. The same profile, written under one set of class
/// ids and read under a completely different set, still lands on the right
/// method — which is the single most important property of the format, and
/// the one a `class_id`-keyed format would get wrong every single run.
#[test]
fn identity_survives_a_different_class_id_space() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));

    // A second run that numbered its classes from 9000 instead of 100.
    let restored = ProfileStore::new();
    let loaded = load_into_with_policy(
        &restored,
        &bytes,
        &resolver(9000),
        &live_len,
        &ReplaySeedPolicy::identity(),
    );
    assert!(
        loaded.is_ok(),
        "load under a different id space must succeed"
    );

    assert_eq!(
        view(&source, 100),
        view(&restored, 9000),
        "the same method, under different ids, must carry the same profile",
    );

    // Concretely: the receiver seeded at bci 11 must be THIS run's
    // `org/example/Impl` (9003), not the id the file's producer used (103).
    let service = key(9002, "handle", "(Ljava/lang/Object;)V");
    let profile = match restored.get_profile(&service) {
        Some(p) => p,
        None => {
            assert!(false, "the method was not seeded under the new id space");
            return;
        }
    };
    let site = match profile.receivers.get(&11) {
        Some(s) => s,
        None => {
            assert!(false, "bci 11 carries no receiver profile");
            return;
        }
    };
    assert_eq!(site.get(&9003).copied(), Some(40));
    assert_eq!(
        site.get(&103),
        None,
        "the producing run's raw class id must never appear in this run's store",
    );
}

/// An empty store is a valid file, and reading it back is "no hints" rather
/// than an error.
#[test]
fn an_empty_profile_is_no_hints_not_an_error() {
    let empty = ProfileStore::new();
    let (bytes, save) = serialize(&empty, &namer(100));
    assert_eq!(save.methods_written, 0);

    let target = ProfileStore::new();
    match load_into_with_policy(
        &target,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    ) {
        Ok(census) => {
            assert_eq!(census.methods_in_file, 0);
            assert_eq!(census.methods_seeded, 0);
            assert!(!census.file_missing);
        }
        Err(e) => assert!(false, "an empty profile must load: {e}"),
    }
    assert_eq!(replay_outcome(&target), ReplayOutcome::default());
}

/// A missing file is "no hints", not an error, at the call site.
#[test]
fn a_missing_file_is_no_hints_not_an_error() {
    let store = ProfileStore::new();
    let mut path = std::env::temp_dir();
    path.push("cratonvm-profile-store-does-not-exist-20260916.bin");
    let _ = std::fs::remove_file(&path);
    match load_from_path(&store, &path, &resolver(100), &live_len) {
        Ok(census) => {
            assert!(census.file_missing);
            assert_eq!(census.methods_seeded, 0);
        }
        Err(e) => assert!(false, "a missing file must not be an error: {e}"),
    }
}

/// r11-tier: a path that is not a regular file is refused up front instead
/// of being read to an end-of-file it may never have (`/dev/zero`, a FIFO).
/// A directory stands in for them portably.
#[test]
fn a_path_that_is_not_a_regular_file_is_refused() {
    let store = ProfileStore::new();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    match load_from_path(&store, dir, &resolver(100), &live_len) {
        Err(ProfileStoreError::Format(m)) => assert!(m.contains("not a regular file"), "{m}"),
        Err(e) => assert!(false, "a directory must be refused as one: {e}"),
        Ok(census) => assert!(false, "a directory must not load: {census:?}"),
    }
}

/// A method whose class is not defined at load time waits for the class's
/// definition and is seeded then -- the load happens before any
/// application class exists, so dropping it dropped the application
/// profile.
#[test]
fn a_method_whose_class_is_defined_later_is_seeded_at_definition() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    // Load time: only bootstrap `java/lang/Object` and `HashMap` exist.
    let at_load = |name: &str| -> Option<u32> {
        match name {
            "java/lang/Object" => Some(200),
            "java/util/HashMap" => Some(201),
            _ => None,
        }
    };
    let census = load_into_with_policy(
        &target,
        &bytes,
        &at_load,
        &live_len,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!(census.methods_seeded, 0);
    assert_eq!(census.methods_unresolved_class, 2);
    assert_eq!(replay_pending_census(&target), (2, 2, 0, 0, 0));
    let service = key(202, "handle", "(Ljava/lang/Object;)V");
    assert!(
        target
            .get_profile(&service)
            .map_or(true, |p| p.branches.is_empty()),
        "nothing seeded before the definition"
    );

    // A definition of an unrelated class seeds nothing.
    assert_eq!(
        seed_pending_replay_for_class(
            &target,
            "org/example/Unrelated",
            &resolver(200),
            &live_len
        ),
        0
    );
    // `Service` is defined; its receiver `Impl` is not yet.
    let after_service = |name: &str| -> Option<u32> {
        match name {
            "org/example/Impl" => None,
            other => resolver(200)(other),
        }
    };
    assert_eq!(
        seed_pending_replay_for_class(
            &target,
            "org/example/Service",
            &after_service,
            &live_len
        ),
        1
    );
    let (classes, methods, seeded, receivers_dropped, _) = replay_pending_census(&target);
    assert_eq!((classes, methods, seeded), (1, 1, 1));
    assert!(
        receivers_dropped > 0,
        "the receiver named an undefined class"
    );
    let seeded_profile = target.get_profile(&service).expect("seeded at definition");
    assert!(!seeded_profile.branches.is_empty());
    // A repeat definition hook finds nothing left.
    assert_eq!(
        seed_pending_replay_for_class(
            &target,
            "org/example/Service",
            &resolver(200),
            &live_len
        ),
        0
    );

    // The sweep form picks up the rest once `Impl` resolves.
    assert_eq!(seed_pending_replay(&target, &resolver(200), &live_len), 1);
    assert_eq!(
        replay_pending_census(&target),
        (0, 0, 2, receivers_dropped, 0)
    );
    assert!(target.get_profile(&key(203, "run", "()I")).is_some());
}

/// An ambiguous (or still unresolvable) name leaves its records waiting.
#[test]
fn an_unresolvable_definition_leaves_the_records_waiting() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    let nothing = |_: &str| -> Option<u32> { None };
    load_into_with_policy(
        &target,
        &bytes,
        &nothing,
        &live_len,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!(
        seed_pending_replay_for_class(&target, "org/example/Service", &nothing, &live_len),
        0
    );
    assert_eq!(replay_pending_census(&target), (2, 2, 0, 0, 0));
}

/// A method whose class this run never loaded is not seeded, is counted,
/// and costs the rest of the file nothing.
#[test]
fn a_method_naming_an_unloaded_class_is_dropped_and_counted() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    // A resolver that only knows `org/example/Impl`.
    let partial = |name: &str| -> Option<u32> {
        if name == "org/example/Impl" {
            Some(7)
        } else {
            None
        }
    };
    match load_into_with_policy(
        &target,
        &bytes,
        &partial,
        &live_len,
        &ReplaySeedPolicy::identity(),
    ) {
        Ok(census) => {
            assert_eq!(census.methods_seeded, 1, "only Impl.run should be seeded");
            assert_eq!(census.methods_unresolved_class, 1);
        }
        Err(e) => assert!(false, "a partially resolvable file must still load: {e}"),
    }
}

// ── stale records (version 2) ───────────────────────────────────────────

/// r11-tier-profile-replay-residuals item 4: a record saved against
/// bytecode of a different length is dropped and counted, at both seeding
/// doors, and the matching record beside it is still seeded.
#[test]
fn a_record_saved_against_other_bytecode_is_dropped_as_stale() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let decoded = decode(&bytes).expect("decodes");
    assert_eq!(
        decoded.iter().map(|r| r.code_len).collect::<Vec<_>>(),
        vec![8, 64],
        "the stamped lengths are written (Impl.run, then Service.handle)"
    );

    // `handle` grew by one byte since the save.
    let grown = |_: u32, name: &str, _: &str| -> LiveCode {
        match name {
            "handle" => LiveCode::Len(65),
            _ => LiveCode::Len(8),
        }
    };
    let eager = ProfileStore::new();
    let census = load_into_with_policy(
        &eager,
        &bytes,
        &resolver(100),
        &grown,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!((census.methods_seeded, census.methods_stale_code), (1, 1));
    assert!(eager
        .get_profile(&key(102, "handle", "(Ljava/lang/Object;)V"))
        .is_none());
    assert!(eager.get_profile(&key(103, "run", "()I")).is_some());

    // The definition door: `run` no longer exists in the live class.
    let deferred = ProfileStore::new();
    let nothing = |_: &str| -> Option<u32> { None };
    load_into_with_policy(
        &deferred,
        &bytes,
        &nothing,
        &live_len,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    let gone = |_: u32, _: &str, _: &str| LiveCode::Absent;
    assert_eq!(
        seed_pending_replay_for_class(&deferred, "org/example/Impl", &resolver(200), &gone),
        0
    );
    assert_eq!(replay_pending_census(&deferred), (1, 1, 0, 0, 1));
}

/// A record whose saving run did not know the length (`code_len` 0) is
/// still refused when a pc lies past the live code; a busy lookup leaves
/// the record waiting instead of dropping it.
#[test]
fn an_unknown_length_is_bounded_by_its_pcs_and_a_busy_lookup_waits() {
    let source = ProfileStore::new();
    source.with_method_profile_mut(&key(102, "spin", "()V"), |p| {
        p.record_branch(40, true);
    });
    let (bytes, _) = serialize(&source, &namer(100));
    assert_eq!(decode(&bytes).expect("decodes")[0].code_len, 0);

    let short = |_: u32, _: &str, _: &str| LiveCode::Len(16);
    let target = ProfileStore::new();
    let census = load_into_with_policy(
        &target,
        &bytes,
        &resolver(100),
        &short,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!((census.methods_seeded, census.methods_stale_code), (0, 1));

    let busy = |_: u32, _: &str, _: &str| LiveCode::Busy;
    let waiting = ProfileStore::new();
    let census = load_into_with_policy(
        &waiting,
        &bytes,
        &resolver(100),
        &busy,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!(
        (
            census.methods_seeded,
            census.methods_stale_code,
            census.methods_unresolved_class
        ),
        (0, 0, 1)
    );
    assert_eq!(
        seed_pending_replay_for_class(&waiting, "org/example/Service", &resolver(100), &busy),
        0
    );
    assert_eq!(
        replay_pending_census(&waiting),
        (1, 1, 0, 0, 0),
        "put back, not dropped"
    );
    assert_eq!(
        seed_pending_replay_for_class(
            &waiting,
            "org/example/Service",
            &resolver(100),
            &live_len
        ),
        1
    );
    let seeded = waiting
        .get_profile(&key(102, "spin", "()V"))
        .expect("seeded once the lookup answered");
    assert_eq!(
        seeded.recorded_code_len(),
        256,
        "the seed stamps the live length"
    );
}

/// The fast-path branch counters stamp the length; two recorders that
/// disagree about it mark the profile, and such a record never seeds.
#[test]
fn a_profile_mixing_two_code_lengths_is_saved_but_never_seeded() {
    let source = ProfileStore::new();
    source.with_method_profile_mut(&key(102, "hot", "()V"), |p| {
        p.flat_branches_for(20).record(3, true);
    });
    source.with_method_profile_mut(&key(102, "mixed", "()V"), |p| {
        p.flat_branches_for(20).record(3, true);
        let _ = p.flat_branches_for(24);
    });
    let (bytes, _) = serialize(&source, &namer(100));
    let lens: Vec<u32> = decode(&bytes)
        .expect("decodes")
        .iter()
        .map(|r| r.code_len)
        .collect();
    assert_eq!(lens, vec![20, crate::profile::CODE_LEN_CONFLICT]);

    let any = |_: u32, _: &str, _: &str| LiveCode::Len(20);
    let target = ProfileStore::new();
    let census = load_into_with_policy(
        &target,
        &bytes,
        &resolver(100),
        &any,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!((census.methods_seeded, census.methods_stale_code), (1, 1));
}

/// A version 1 file is refused with the reason, not as a generic unknown
/// version.
#[test]
fn a_version_1_file_is_refused_with_its_reason() {
    let mut bytes = one_of_everything();
    bytes[4] = 1;
    bytes[5] = 0;
    let message = err_of(&bytes);
    assert!(
        message.contains("version 1 carries no per-method code length"),
        "expected the version-1 refusal, got {message:?}",
    );
}

// ── exact records (version 3) ───────────────────────────────────────────

/// The hash is FNV-1a 64 and must stay it: a file written by one build
/// is read by another, so the digest is part of the format.
#[test]
fn the_bytecode_hash_is_fnv1a_64_and_never_zero() {
    assert_eq!(bytecode_hash(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(bytecode_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_ne!(bytecode_hash(&[0x2a, 0xb7]), bytecode_hash(&[0x2b, 0xb7]));
}

/// Round 11 wave 14 (r11-tier-profile-replay-residuals item 4, the hash
/// half): a save stamps each record with the saving run's live method, and
/// a seeding door drops a record whose bytes changed at the same length.
/// A door that knows only the length still seeds it.
#[test]
fn a_same_length_edit_is_dropped_by_the_hash() {
    let source = populated(100);
    let saved_code = |_: u32, name: &str, _: &str| -> LiveCode {
        match name {
            "handle" => LiveCode::Code {
                len: 64,
                hash: 0x1111,
            },
            _ => LiveCode::Code {
                len: 8,
                hash: 0x2222,
            },
        }
    };
    let (bytes, _) = serialize_against_live_code(&source, &namer(100), &saved_code);
    let decoded = decode(&bytes).expect("decodes");
    assert_eq!(
        decoded
            .iter()
            .map(|r| (r.code_len, r.code_hash))
            .collect::<Vec<_>>(),
        vec![(8, 0x2222), (64, 0x1111)],
        "Impl.run, then Service.handle"
    );

    // `handle` was edited in place: same length, other bytes.
    let edited = |_: u32, name: &str, _: &str| -> LiveCode {
        match name {
            "handle" => LiveCode::Code {
                len: 64,
                hash: 0x3333,
            },
            _ => LiveCode::Code {
                len: 8,
                hash: 0x2222,
            },
        }
    };
    let target = ProfileStore::new();
    let census = load_into_with_policy(
        &target,
        &bytes,
        &resolver(100),
        &edited,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!((census.methods_seeded, census.methods_stale_code), (1, 1));
    assert!(target
        .get_profile(&key(102, "handle", "(Ljava/lang/Object;)V"))
        .is_none());

    // A length-only answer cannot see the edit, so both seed.
    let length_only = ProfileStore::new();
    let census = load_into_with_policy(
        &length_only,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!((census.methods_seeded, census.methods_stale_code), (2, 0));
}

/// A record no length-aware recorder touched (receiver evidence only) is
/// stamped from the live method at save; a record recorded against
/// another length than the live one is written as a conflict and never
/// seeds.
#[test]
fn a_save_stamps_unknown_lengths_live_and_refuses_another_version() {
    let source = ProfileStore::new();
    source.with_method_profile_mut(&key(102, "dispatch", "()V"), |p| {
        p.record_receiver(4, 103);
    });
    source.with_method_profile_mut(&key(102, "old", "()V"), |p| {
        p.note_code_len(8);
        p.record_branch(2, true);
    });
    let live = |_: u32, name: &str, _: &str| -> LiveCode {
        match name {
            "dispatch" => LiveCode::Code {
                len: 12,
                hash: 0x44,
            },
            _ => LiveCode::Code {
                len: 16,
                hash: 0x55,
            },
        }
    };
    let (bytes, _) = serialize_against_live_code(&source, &namer(100), &live);
    let decoded = decode(&bytes).expect("decodes");
    assert_eq!(
        decoded
            .iter()
            .map(|r| (r.method_name.as_str(), r.code_len, r.code_hash))
            .collect::<Vec<_>>(),
        vec![
            ("dispatch", 12, 0x44),
            ("old", crate::profile::CODE_LEN_CONFLICT, 0),
        ]
    );
    // A busy or absent live answer keeps what the recorders stamped.
    assert_eq!(stamp_record_code(8, LiveCode::Busy), (8, 0));
    assert_eq!(stamp_record_code(0, LiveCode::Absent), (0, 0));
    assert_eq!(stamp_record_code(8, LiveCode::Len(8)), (8, 0));
}

/// A version 2 file (length, no hash) is still read: its records keep the
/// length check and seed against a hashing door.
#[test]
fn a_version_2_file_is_read_with_every_hash_unknown() {
    let source = populated(100);
    let mut census = SaveCensus::default();
    let records = collect(&source, &namer(100), &mut census);
    let v2 = encode_as(&records, 2);
    let decoded = decode(&v2).expect("a version 2 file decodes");
    assert!(decoded.iter().all(|r| r.code_hash == 0));
    assert_eq!(
        decoded.iter().map(|r| r.code_len).collect::<Vec<_>>(),
        vec![8, 64]
    );
    let hashing = |_: u32, name: &str, _: &str| -> LiveCode {
        match name {
            "handle" => LiveCode::Code {
                len: 64,
                hash: 0x77,
            },
            _ => LiveCode::Code { len: 8, hash: 0x88 },
        }
    };
    let target = ProfileStore::new();
    let loaded = load_into_with_policy(
        &target,
        &v2,
        &resolver(100),
        &hashing,
        &ReplaySeedPolicy::identity(),
    )
    .expect("loads");
    assert_eq!((loaded.methods_seeded, loaded.methods_stale_code), (2, 0));
    // Every truncation of a version 2 file is refused too.
    for cut in 0..v2.len() {
        assert!(
            decode(&v2[..cut]).is_err(),
            "v2 truncated to {cut} accepted"
        );
    }
}

// ── hostile input ───────────────────────────────────────────────────────

fn header(method_count: u32) -> Vec<u8> {
    let mut b = Vec::new();
    put_u32(&mut b, MAGIC);
    put_u16(&mut b, VERSION);
    put_u16(&mut b, RESERVED);
    put_u32(&mut b, method_count);
    b
}

fn err_of(bytes: &[u8]) -> String {
    match decode(bytes) {
        Ok(_) => String::new(),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_bad_magic_is_refused() {
    let mut bytes = one_of_everything();
    bytes[0] ^= 0xFF;
    let message = err_of(&bytes);
    assert!(
        message.contains("bad magic"),
        "expected a magic refusal, got {message:?}",
    );
}

#[test]
fn a_bad_version_is_refused() {
    let mut bytes = one_of_everything();
    bytes[4] = 0xEE;
    bytes[5] = 0xFF;
    let message = err_of(&bytes);
    assert!(
        message.contains("unsupported profile version"),
        "expected a version refusal, got {message:?}",
    );
}

#[test]
fn a_non_zero_reserved_field_is_refused() {
    let mut bytes = one_of_everything();
    bytes[6] = 1;
    let message = err_of(&bytes);
    assert!(
        message.contains("reserved header field"),
        "expected a reserved-field refusal, got {message:?}",
    );
}

/// `u32::MAX` methods is refused at the four bytes that declare it, by the
/// `MAX_METHODS` cap, before any collection is sized from it.
#[test]
fn a_method_count_of_u32_max_is_refused_at_the_cap() {
    let blob = header(u32::MAX);
    let message = err_of(&blob);
    assert!(
        message.contains("exceeds this format's cap"),
        "expected the cap to refuse it, got {message:?}",
    );
}

/// `u32::MAX` receiver types is refused the same way. Before the cap
/// existed this would have asked the allocator for
/// 4 294 967 295 × `size_of::<(String, u32)>()` — about 137 GiB — from a
/// few dozen bytes of input.
#[test]
fn a_receiver_type_count_of_u32_max_is_refused_at_the_cap() {
    let mut body = Vec::new();
    put_str(&mut body, "org/example/Service");
    put_str(&mut body, "handle");
    put_str(&mut body, "()V");
    put_u32(&mut body, 0); // code_len
    put_u64(&mut body, 0); // code_hash (version 3)
    put_u32(&mut body, 0); // invocations
    put_u32(&mut body, 0); // branches
    put_u32(&mut body, 0); // call sites
    put_u32(&mut body, 0); // loops
    put_u32(&mut body, 1); // one receiver site
    put_u32(&mut body, 11); // pc
    put_u32(&mut body, u32::MAX); // ← the hostile count

    let mut blob = header(1);
    put_u32(&mut blob, body.len() as u32);
    blob.extend_from_slice(&body);

    let message = err_of(&blob);
    assert!(
        message.contains("receiver type count") && message.contains("cap"),
        "expected the receiver-type cap to refuse it, got {message:?}",
    );
}

/// A count that passes its `MAX_*` cap but is larger than the bytes behind
/// it is refused by the bytes-remaining bound. This is the bound that
/// actually holds, and the only one that makes the surviving
/// `Vec::with_capacity` safe rather than merely unlikely to hurt.
#[test]
fn a_count_inside_the_cap_but_past_the_bytes_is_refused() {
    // 4 096 receiver types is exactly MAX_RECEIVER_TYPES_PER_SITE, so the
    // cap passes it and only the second bound can refuse it.
    let mut body = Vec::new();
    put_str(&mut body, "org/example/Service");
    put_str(&mut body, "handle");
    put_str(&mut body, "()V");
    put_u32(&mut body, 0); // code_len
    put_u64(&mut body, 0); // code_hash (version 3)
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 1);
    put_u32(&mut body, 11);
    put_u32(&mut body, MAX_RECEIVER_TYPES_PER_SITE as u32);
    // ...and then nothing. 4 096 types need at least 32 768 bytes.

    let mut blob = header(1);
    put_u32(&mut blob, body.len() as u32);
    blob.extend_from_slice(&body);

    let message = err_of(&blob);
    assert!(
        message.contains("cannot fit in the"),
        "expected the bytes-remaining bound to refuse it, got {message:?}",
    );
}

/// The same, one level up: a method count inside `MAX_METHODS` but larger
/// than `remaining / MIN_METHOD_BYTES`.
#[test]
fn a_method_count_inside_the_cap_but_past_the_bytes_is_refused() {
    let blob = header(1000);
    let message = err_of(&blob);
    assert!(
        message.contains("cannot fit in the"),
        "expected the bytes-remaining bound to refuse it, got {message:?}",
    );
}

/// A declared string length beyond the `CONSTANT_Utf8` ceiling is refused
/// at the length, before the cursor is advanced by it. On a 32-bit target
/// this is also the case that would wrap `pos + len` without `checked_add`.
#[test]
fn a_u32_max_string_length_is_refused_at_the_length() {
    let mut body = Vec::new();
    put_u32(&mut body, u32::MAX); // class name length
                                  // Pad so the record is at least `MIN_METHOD_BYTES` long. Without the
                                  // padding the METHOD count's bytes-remaining bound refuses the file
                                  // first, and this test would pass while checking a different bound.
    body.resize(MIN_METHOD_BYTES - 4, 0);
    let mut blob = header(1);
    put_u32(&mut blob, body.len() as u32);
    blob.extend_from_slice(&body);
    let message = err_of(&blob);
    assert!(
        message.contains("exceeds the") && message.contains("cap"),
        "expected the string cap to refuse it, got {message:?}",
    );
}

/// Invalid UTF-8 is an `Err`, never a panic.
#[test]
fn invalid_utf8_is_an_error() {
    let mut body = Vec::new();
    put_u32(&mut body, 4);
    body.extend_from_slice(&[0xF0, 0x28, 0x8C, 0x28]); // not UTF-8
    put_str(&mut body, "handle");
    put_str(&mut body, "()V");
    put_u32(&mut body, 0); // code_len
    put_u64(&mut body, 0); // code_hash (version 3)
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);

    let mut blob = header(1);
    put_u32(&mut blob, body.len() as u32);
    blob.extend_from_slice(&body);

    let message = err_of(&blob);
    assert!(
        message.contains("not UTF-8"),
        "expected a UTF-8 refusal, got {message:?}",
    );
}

/// **Every** prefix of a well-formed file is an error. Not a sample of
/// prefixes: every one, because a truncation bound that holds at some
/// offsets is a truncation bound that does not hold.
#[test]
fn every_truncation_is_an_error() {
    let bytes = one_of_everything();
    assert!(bytes.len() > 64, "the fixture must exercise several arrays");
    for cut in 0..bytes.len() {
        assert!(
            decode(&bytes[..cut]).is_err(),
            "a file truncated to {cut} of {} byte(s) was accepted",
            bytes.len(),
        );
    }
    assert!(
        decode(&bytes).is_ok(),
        "the untruncated fixture must still parse",
    );
}

/// Trailing bytes after the last record are refused. Version 1 is strict
/// about this; the module doc says why, and says which future change
/// relaxes it.
#[test]
fn trailing_bytes_are_refused() {
    let mut bytes = one_of_everything();
    bytes.push(0);
    let message = err_of(&bytes);
    assert!(
        message.contains("trailing byte"),
        "expected a trailing-byte refusal, got {message:?}",
    );
}

/// A record that declares more bytes than its fields consume is refused,
/// which is what makes the length delimiter a check rather than decoration.
#[test]
fn a_record_longer_than_its_fields_is_refused() {
    let mut body = Vec::new();
    put_str(&mut body, "org/example/Service");
    put_str(&mut body, "handle");
    put_str(&mut body, "()V");
    put_u32(&mut body, 0); // code_len
    put_u64(&mut body, 0); // code_hash (version 3)
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    body.push(0xAA); // one byte no field claims

    let mut blob = header(1);
    put_u32(&mut blob, body.len() as u32);
    blob.extend_from_slice(&body);

    let message = err_of(&blob);
    assert!(
        message.contains("must be exactly its declared length"),
        "expected a record-length refusal, got {message:?}",
    );
}

/// A corrupt count inside one record cannot reach past that record: the
/// error names the record, and the bound that refused it is the record's
/// own length rather than the file's.
#[test]
fn a_corrupt_count_is_contained_within_its_record() {
    let mut bytes = one_of_everything();
    // The header is 12 bytes (magic, version, reserved, method count), so
    // the first record's length prefix starts there. The last four bytes of
    // that record's body are its final array count — the receiver-site
    // count — and clobbering them with `u32::MAX` is a corruption strictly
    // INSIDE record 0. The point of the assertion is the prefix on the
    // message: the failure is attributed to the record, which is what the
    // length delimiter buys.
    let record_len_at = 12;
    let len_bytes = [
        bytes[record_len_at],
        bytes[record_len_at + 1],
        bytes[record_len_at + 2],
        bytes[record_len_at + 3],
    ];
    let record_len = u32::from_le_bytes(len_bytes) as usize;
    let last_count_at = record_len_at + 4 + record_len - 4;
    bytes[last_count_at..last_count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let message = err_of(&bytes);
    assert!(
        message.contains("method record 0"),
        "the error must name the record it came from, got {message:?}",
    );
}

// ── replay is a hint ────────────────────────────────────────────────────

/// A replayed receiver is marked `from_replay`; a live observation of the
/// SAME class confirms it and the census moves.
#[test]
fn a_confirmed_receiver_keeps_its_seed_and_moves_the_census() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    let _ = load_into_with_policy(
        &target,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    );

    let service = key(102, "handle", "(Ljava/lang/Object;)V");
    let before = replay_outcome(&target);
    assert!(before.methods_seeded > 0);
    assert_eq!(before.receiver_types_confirmed, 0);
    assert_eq!(before.receiver_types_refuted, 0);
    assert_eq!(
        before.receiver_types_unjudged, before.receiver_types_replayed,
        "nothing has executed yet, so nothing is judged",
    );

    // The live run sees `org/example/Impl` (103) at bci 11 — the class the
    // replay predicted.
    target.with_method_profile_mut(&service, |p| {
        assert!(
            p.is_from_replay(),
            "the seeded method must carry provenance"
        );
        p.record_receiver(11, 103);
    });

    let after = replay_outcome(&target);
    assert_eq!(after.receiver_types_confirmed, 1);
    assert_eq!(after.receiver_types_refuted, 0);
    assert_eq!(after.methods_shape_contradicted, 0);

    // The seed survives confirmation — that is the point of confirming it.
    match target
        .get_profile(&service)
        .and_then(|p| p.receivers.get(&11).map(|s| s.get(&103).copied()))
    {
        Some(Some(n)) => assert_eq!(n, 41, "40 seeded plus the one live observation"),
        _ => assert!(false, "the confirmed seed must still be there"),
    }
}

/// r11-tier: a SECOND observation of an already-confirmed class is not a
/// contradiction. Confirming `Impl` took it off the pending list, so before
/// the fix the next `Impl` read as a class the replay never named and
/// refuted `Other`'s tail seed — every replayed bimorphic site lost its
/// tail on its first repeat.
#[test]
fn a_confirmed_class_seen_again_refutes_nothing() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    let _ = load_into_with_policy(
        &target,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    );

    let service = key(102, "handle", "(Ljava/lang/Object;)V");
    target.with_method_profile_mut(&service, |p| {
        p.record_receiver(11, 103);
        p.record_receiver(11, 103);
        p.record_receiver(11, 103);
    });

    let after = replay_outcome(&target);
    assert_eq!(after.receiver_types_confirmed, 1);
    assert_eq!(
        after.receiver_types_refuted, 0,
        "Impl was named by the replay"
    );
    assert_eq!(after.methods_shape_contradicted, 0);
    assert_eq!(
        target
            .get_profile(&service)
            .and_then(|p| p.receivers.get(&11).and_then(|s| s.get(&104).copied())),
        Some(3),
        "Other's tail seed must survive a repeat of the confirmed class",
    );

    // A class the replay never named still refutes what is pending.
    target.with_method_profile_mut(&service, |p| p.record_receiver(11, 101));
    let refuted = replay_outcome(&target);
    assert_eq!(
        refuted.receiver_types_refuted, 1,
        "only Other was still pending"
    );
    assert_eq!(refuted.methods_shape_contradicted, 1);
}

/// A live contradiction CORRECTS the replayed shape: the still-pending
/// seeds at that site are subtracted back out, the method is marked
/// contradicted, and the census moves from unjudged to refuted.
#[test]
fn a_live_contradiction_corrects_the_replayed_shape() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    let _ = load_into_with_policy(
        &target,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    );

    let service = key(102, "handle", "(Ljava/lang/Object;)V");
    // The live run sees `java/util/HashMap` (101) at bci 11 — a class the
    // replay never named there.
    target.with_method_profile_mut(&service, |p| p.record_receiver(11, 101));

    let after = replay_outcome(&target);
    assert_eq!(
        after.receiver_types_refuted, 2,
        "both seeds at bci 11 (Impl and Other) are refuted",
    );
    assert_eq!(after.methods_shape_contradicted, 1);
    assert_eq!(after.receiver_types_confirmed, 0);

    let profile = match target.get_profile(&service) {
        Some(p) => p,
        None => {
            assert!(false, "the method must still exist");
            return;
        }
    };
    let site = match profile.receivers.get(&11) {
        Some(s) => s,
        None => {
            assert!(false, "the live observation must keep the site alive");
            return;
        }
    };
    assert_eq!(
        site.get(&103),
        None,
        "the refuted seed must be subtracted back out, not averaged in",
    );
    assert_eq!(site.get(&104), None, "and so must the tail seed");
    assert_eq!(
        site.get(&101).copied(),
        Some(1),
        "the live observation that did the refuting must survive",
    );

    // A second contradicting observation must not re-count: both seeds have
    // already been judged.
    target.with_method_profile_mut(&service, |p| p.record_receiver(11, 101));
    assert_eq!(replay_outcome(&target).receiver_types_refuted, 2);
}

/// A site the replay did not name at all is untouched by replay accounting:
/// a live observation there is neither a confirmation nor a refutation.
#[test]
fn an_unseeded_site_is_not_replay_accounted() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    let _ = load_into_with_policy(
        &target,
        &bytes,
        &resolver(100),
        &live_len,
        &ReplaySeedPolicy::identity(),
    );
    let service = key(102, "handle", "(Ljava/lang/Object;)V");
    target.with_method_profile_mut(&service, |p| p.record_receiver(999, 101));
    let after = replay_outcome(&target);
    assert_eq!(after.receiver_types_confirmed, 0);
    assert_eq!(after.receiver_types_refuted, 0);
    assert_eq!(after.methods_shape_contradicted, 0);
}

// ── the seeding policy ──────────────────────────────────────────────────

/// A replayed invocation count can never, on its own, reach the gate it
/// feeds. This is the claim `ReplaySeedPolicy::conservative` makes, and it
/// has to hold at every gate, including the small ones a bisecting
/// developer sets.
#[test]
fn a_replayed_invocation_count_cannot_reach_the_gate_alone() {
    for gate in [1u32, 2, 4, 5, 9, 50, 500, 20_000, u32::MAX] {
        let policy = ReplaySeedPolicy::conservative(gate);
        assert!(
            policy.invocation_ceiling < gate.max(1) || gate <= 1,
            "ceiling {} must be below gate {gate}",
            policy.invocation_ceiling,
        );
        for recorded in [0u32, 1, 499, 500, 1_000_000, u32::MAX] {
            let seeded = policy.seed_invocations(recorded);
            assert!(
                seeded <= policy.invocation_ceiling,
                "seed {seeded} exceeded the ceiling at gate {gate}",
            );
            if gate > 1 {
                assert!(
                    seeded < gate,
                    "a seed of {seeded} would reach the gate of {gate} on its own",
                );
            }
        }
    }
}

/// At the default gate the seed is a quarter of the recorded count, capped
/// at a fifth of the gate — so a method that made 888 calls last run starts
/// this run needing 400 real ones instead of 500.
#[test]
fn the_default_policy_seeds_a_bounded_fraction() {
    let policy = ReplaySeedPolicy::conservative(500);
    assert_eq!(policy.invocation_ceiling, 100);
    assert_eq!(policy.seed_invocations(888), 100);
    assert_eq!(policy.seed_invocations(200), 50);
    assert_eq!(policy.seed_invocations(3), 0);

    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    match load_into_with_policy(&target, &bytes, &resolver(100), &live_len, &policy) {
        Ok(census) => assert_eq!(census.invocations_seeded, 100),
        Err(e) => assert!(false, "load failed: {e}"),
    }
    let packed = cratonvm_jit_api::invoc_key_parts(102, "handle", "(Ljava/lang/Object;)V");
    // `add_invocations(key, 0)` is the read: it returns the counter's value
    // after adding nothing, which is the seeded credit. The store has no
    // plain getter and does not need one for the live path.
    assert_eq!(target.add_invocations(packed, 0), 100);
    assert_eq!(replay_outcome(&target).invocations_seeded, 100);
}

/// A replayed branch hint is exactly as revisable as a live one: the
/// ceiling is low enough that live observations outvote it.
#[test]
fn a_replayed_branch_hint_is_revisable() {
    let source = ProfileStore::new();
    let hot = key(102, "loop", "()V");
    source.with_method_profile_mut(&hot, |p| {
        // Last run: overwhelmingly taken, a million observations.
        p.seed_replayed_branch(4, 1_000_000, 0);
        // A SOURCE store: the seed stands in for last run's live evidence,
        // which a save writes (a save subtracts replayed seeds).
        p.replay = None;
    });
    let (bytes, _) = serialize(&source, &namer(100));

    let target = ProfileStore::new();
    let policy = ReplaySeedPolicy::conservative(500);
    let _ = load_into_with_policy(&target, &bytes, &resolver(100), &live_len, &policy);

    let seeded = match target
        .get_profile(&hot)
        .and_then(|p| p.branches.get(&4).cloned())
    {
        Some(b) => b,
        None => {
            assert!(false, "the branch must be seeded");
            return;
        }
    };
    assert_eq!(seeded.taken, policy.branch_ceiling);
    assert!(seeded.is_usually_taken());

    // This run's traffic goes the other way. Twice the ceiling of live
    // not-taken observations flips the hint.
    target.with_method_profile_mut(&hot, |p| {
        for _ in 0..(policy.branch_ceiling * 2) {
            p.record_branch(4, false);
        }
    });
    let now = match target
        .get_profile(&hot)
        .and_then(|p| p.branches.get(&4).cloned())
    {
        Some(b) => b,
        None => {
            assert!(false, "the branch must still be there");
            return;
        }
    };
    assert!(
        !now.is_usually_taken(),
        "live observation must be able to outvote a replayed branch hint",
    );
}

/// r11-tier: a capped seed keeps its RATIO. Capping each direction (and
/// each receiver class) on its own seeded a 5 000 : 1 branch as 50/50 and a
/// 200 : 1 monomorphic site as an even bimorphic one.
#[test]
fn capping_a_seed_preserves_its_ratio() {
    assert_eq!(scale_to_ceiling(1_000_000, 1_000_000, 100), 100);
    assert_eq!(
        scale_to_ceiling(200, 1_000_000, 100),
        1,
        "non-zero stays non-zero"
    );
    assert_eq!(scale_to_ceiling(0, 1_000_000, 100), 0);
    assert_eq!(
        scale_to_ceiling(40, 60, 100),
        40,
        "under the ceiling: unchanged"
    );
    assert_eq!(scale_to_ceiling(u32::MAX, u32::MAX, 1_000), 1_000);

    let source = ProfileStore::new();
    let hot = key(102, "branchy", "()V");
    source.with_method_profile_mut(&hot, |p| {
        p.seed_replayed_branch(4, 1_000_000, 200_000);
        // A SOURCE store (see `a_replayed_branch_hint_is_revisable`).
        p.replay = None;
        for _ in 0..5_000 {
            p.record_receiver(9, 103);
        }
        for _ in 0..25 {
            p.record_receiver(9, 104);
        }
    });
    let (bytes, _) = serialize(&source, &namer(100));
    let target = ProfileStore::new();
    let policy = ReplaySeedPolicy::conservative(500);
    let _ = load_into_with_policy(&target, &bytes, &resolver(100), &live_len, &policy);

    let profile = match target.get_profile(&hot) {
        Some(p) => p,
        None => {
            assert!(false, "the method must be seeded");
            return;
        }
    };
    match profile.branches.get(&4) {
        Some(b) => {
            assert_eq!(b.taken, policy.branch_ceiling);
            assert_eq!(
                b.not_taken,
                policy.branch_ceiling / 5,
                "5 : 1 survives the cap"
            );
        }
        None => assert!(false, "the branch must be seeded"),
    }
    match profile.receivers.get(&9) {
        Some(site) => {
            assert_eq!(site.get(&103).copied(), Some(policy.receiver_ceiling));
            assert_eq!(
                site.get(&104).copied(),
                Some(policy.receiver_ceiling / 200),
                "200 : 1 survives the cap",
            );
        }
        None => assert!(false, "the receiver site must be seeded"),
    }
}

/// The loop seed preserves the AVERAGE trip count when it caps the entry
/// count, because the average is the only thing the unroll heuristic reads.
#[test]
fn capping_a_loop_seed_preserves_its_average() {
    let source = ProfileStore::new();
    let looper = key(102, "spin", "()V");
    source.with_method_profile_mut(&looper, |p| {
        // 50 000 entries averaging 8 trips each.
        p.seed_replayed_loop(3, 400_000, 50_000, 400_000);
        // A SOURCE store (see `a_replayed_branch_hint_is_revisable`).
        p.replay = None;
    });
    let (bytes, _) = serialize(&source, &namer(100));

    let target = ProfileStore::new();
    let policy = ReplaySeedPolicy::conservative(500);
    let _ = load_into_with_policy(&target, &bytes, &resolver(100), &live_len, &policy);

    let lp = match target
        .get_profile(&looper)
        .and_then(|p| p.loops.get(&3).cloned())
    {
        Some(l) => l,
        None => {
            assert!(false, "the loop must be seeded");
            return;
        }
    };
    assert_eq!(lp.entry_count, policy.loop_entry_ceiling);
    assert!(
        (lp.avg_trip_count() - 8.0).abs() < 0.01,
        "capping must preserve the average, got {}",
        lp.avg_trip_count(),
    );
}

/// A receiver seed is capped, so a stale monomorphic reading is outvotable
/// — but the cap is above `INLINE_MIN_SPECULATION_OBSERVATIONS` (250), so
/// replay is not made useless by its own conservatism.
#[test]
fn the_receiver_ceiling_is_usable_but_outvotable() {
    let policy = ReplaySeedPolicy::conservative(500);
    assert!(
        policy.receiver_ceiling > 250,
        "a ceiling at or below the speculation floor makes receiver replay a no-op",
    );
    assert!(
        policy.receiver_ceiling <= 10_000,
        "a ceiling this high stops live traffic being able to outvote the seed",
    );
}

// ── file round trip ─────────────────────────────────────────────────────

/// The whole path, through a real file.
#[test]
fn save_and_load_through_a_file() {
    let source = populated(100);
    let mut path = std::env::temp_dir();
    path.push("cratonvm-profile-store-roundtrip-20260916.bin");
    let _ = std::fs::remove_file(&path);

    match save_to_path(&source, &path, &namer(100), &no_live_code) {
        Ok(census) => assert_eq!(census.methods_written, 2),
        Err(e) => {
            assert!(false, "save failed: {e}");
            return;
        }
    }

    let target = ProfileStore::new();
    match load_from_path(&target, &path, &resolver(9000), &live_len) {
        Ok(census) => {
            assert!(!census.file_missing);
            assert_eq!(census.methods_seeded, 2);
        }
        Err(e) => assert!(false, "load failed: {e}"),
    }
    assert_eq!(
        view_without_invocations(&source, 100),
        view_without_invocations(&target, 9000),
        "every map must survive a real file round trip under new class ids",
    );
    let _ = std::fs::remove_file(&path);
}

/// Round 13 wave 3 (lane replay2; R13-6): a save replaces an existing
/// file whole (through a renamed temporary) and leaves no temporary
/// behind, so a second writer's save can only replace the first one's.
#[test]
fn a_save_replaces_the_file_whole_and_leaves_no_temporary() {
    let source = populated(100);
    let mut path = std::env::temp_dir();
    path.push("cratonvm-profile-store-atomic-20260928.bin");
    // Longer than any encoding: a direct overwrite would not shrink it
    // any differently, but a stale tail must not survive either way.
    if std::fs::write(&path, vec![0xA5u8; 1 << 16]).is_err() {
        return;
    }
    if let Err(e) = save_to_path(&source, &path, &namer(100), &no_live_code) {
        assert!(false, "save failed: {e}");
        return;
    }
    let (bytes, _) = serialize(&source, &namer(100));
    assert_eq!(
        std::fs::read(&path).ok(),
        Some(bytes),
        "exactly one encoding"
    );
    assert!(
        !super::temporary_sibling(&path).is_some_and(|tmp| tmp.exists()),
        "no temporary left"
    );
    let target = ProfileStore::new();
    assert!(load_from_path(&target, &path, &resolver(9000), &live_len).is_ok());
    let _ = std::fs::remove_file(&path);
}

/// Round 13 wave 9 (lane replay5): two threads (two embedded VMs dropped
/// at once) saving to one path write two different temporaries, so
/// neither rename can publish the other's half-written bytes; one thread
/// always gets the same name, beside the target in its directory.
#[test]
fn concurrent_savers_on_two_threads_write_distinct_temporaries() {
    let path = std::env::temp_dir().join("cratonvm-profile-store-tmpname-20260928.bin");
    let here = super::temporary_sibling(&path);
    assert_eq!(here, super::temporary_sibling(&path), "stable per thread");
    let there = std::thread::spawn({
        let path = path.clone();
        move || super::temporary_sibling(&path)
    })
    .join()
    .ok()
    .flatten();
    assert!(here.is_some() && there.is_some());
    assert_ne!(here, there, "one temporary per writing thread");
    assert_eq!(
        here.as_ref()
            .and_then(|tmp| tmp.parent().map(Path::to_path_buf)),
        path.parent().map(Path::to_path_buf),
        "renamed within one directory"
    );
}

/// A truncated file on disk is an `Err`, and the store is still usable.
#[test]
fn a_truncated_file_on_disk_is_an_error_and_the_store_survives() {
    let source = populated(100);
    let (bytes, _) = serialize(&source, &namer(100));
    let mut path = std::env::temp_dir();
    path.push("cratonvm-profile-store-truncated-20260916.bin");
    let cut = bytes.len() / 2;
    if std::fs::write(&path, &bytes[..cut]).is_err() {
        return;
    }
    let target = ProfileStore::new();
    assert!(
        load_from_path(&target, &path, &resolver(100), &live_len).is_err(),
        "a truncated file must be refused",
    );
    let _ = std::fs::remove_file(&path);
}
