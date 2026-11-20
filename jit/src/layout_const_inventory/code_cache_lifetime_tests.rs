use super::*;

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

fn key() -> (Arc<str>, Arc<str>, Arc<str>, cratonvm_types::ClassId) {
    (
        Arc::from("cclt/Subject"),
        Arc::from("m"),
        Arc::from("()V"),
        cratonvm_types::ClassId::new(1),
    )
}

/// One VM's publication or redefinition must not flush another VM's
/// negative memos or inline caches: both counters belong to the cache.
#[test]
fn generation_and_redefine_epoch_belong_to_one_cache() {
    let a = JitCache::new();
    let b = JitCache::new();
    assert_eq!(
        a.generation(),
        1,
        "0 must stay the memos' never-probed value"
    );
    let (class, method, desc, cid) = key();
    let b_before = b.generation();
    a.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    assert!(a.generation() > 1, "a publication advances its own cache");
    assert_eq!(b.generation(), b_before, "and no other");

    let (a_epoch, b_epoch) = (a.redefine_epoch(), b.redefine_epoch());
    a.bump_redefine_epoch();
    assert_ne!(a.redefine_epoch(), a_epoch);
    assert_eq!(b.redefine_epoch(), b_epoch);
}

/// A compilation that inlined from `Base` and began before
/// `invalidate_for_class_change("Base")` must not publish afterwards. The
/// invalidation scanned a cache that did not contain the body yet, so
/// nothing else would ever withdraw it.
#[test]
fn a_body_compiled_before_an_invalidation_of_its_inlined_class_is_refused() {
    let cache = JitCache::new();
    let (class, method, desc, cid) = key();

    let witness = open_compile_epoch_witness();
    let mut stale = ret_body();
    stale
        .inlined_methods
        .push(("cclt/Base".to_string(), "m".to_string(), "()V".to_string()));
    // The hierarchy changes while this compile is still running; nothing
    // published names `cclt/Base` yet, so the scan itself finds nothing.
    assert_eq!(cache.invalidate_for_class_change("cclt/Base"), 0);
    // An invalidation of an unrelated class must not refuse it.
    let mut unrelated = ret_body();
    unrelated.inlined_methods.push((
        "cclt/Other".to_string(),
        "m".to_string(),
        "()V".to_string(),
    ));
    drop(witness);

    cache.put(class.clone(), method.clone(), desc.clone(), cid, stale);
    assert!(
        cache.get(&class, &method, &desc, cid).is_none(),
        "a body compiled against the hierarchy an invalidation retired must not publish"
    );

    let other_method: Arc<str> = Arc::from("other");
    cache.put(
        class.clone(),
        other_method.clone(),
        desc.clone(),
        cid,
        unrelated,
    );
    assert!(
        cache.get(&class, &other_method, &desc, cid).is_some(),
        "an invalidation of a class the body does not depend on must not refuse it"
    );

    // The same dependency compiled AFTER the invalidation publishes.
    let mut fresh = ret_body();
    fresh
        .inlined_methods
        .push(("cclt/Base".to_string(), "m".to_string(), "()V".to_string()));
    cache.put(class.clone(), method.clone(), desc.clone(), cid, fresh);
    assert!(cache.get(&class, &method, &desc, cid).is_some());
}

/// `remove` is an invalidation of one key: a compile of that key that began
/// before the removal must not publish after it.
#[test]
fn a_body_compiled_before_its_key_was_removed_is_refused() {
    let cache = JitCache::new();
    let (class, method, desc, cid) = key();
    let witness = open_compile_epoch_witness();
    let stale = ret_body();
    cache.remove(&class, &method, &desc, cid);
    drop(witness);
    cache.put(class.clone(), method.clone(), desc.clone(), cid, stale);
    assert!(cache.get(&class, &method, &desc, cid).is_none());
}

/// A callee an invalidation already retired may still be alive — held by
/// the retirement queue, or here by the test — so the owner pin succeeds.
/// Baking a call to it must still refuse the caller.
#[test]
fn a_retired_direct_callee_is_never_baked_into_a_publication() {
    let cache = JitCache::new();
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4731);
    let callee_class: Arc<str> = Arc::from("cclt/RetiredCallee");
    let callee_method: Arc<str> = Arc::from("target");
    cache.put(
        callee_class.clone(),
        callee_method.clone(),
        desc.clone(),
        cid,
        ret_body(),
    );
    let callee = cache
        .get(&callee_class, &callee_method, &desc, cid)
        .expect("callee published");
    cache.remove(&callee_class, &callee_method, &desc, cid);
    assert!(
        callee.retired.load(std::sync::atomic::Ordering::SeqCst),
        "an invalidated body is marked retired"
    );

    let mut caller = ret_body();
    caller
        ._direct_callee_entries
        .push(callee.entry_ptr() as usize);
    caller
        ._direct_callee_expected
        .push((callee.entry_ptr() as usize, callee.artifact_id));
    let caller_class: Arc<str> = Arc::from("cclt/RetiredCaller");
    let caller_method: Arc<str> = Arc::from("call");
    cache.put(
        caller_class.clone(),
        caller_method.clone(),
        desc.clone(),
        cid,
        caller,
    );
    assert!(
        cache
            .get(&caller_class, &caller_method, &desc, cid)
            .is_none(),
        "a caller baking a call to a retired callee must not publish"
    );
    drop(callee);
}

/// Read the name registry past a transient `Locked`.
///
/// `lookup_jit_method_name*` is a `try_lock` accessor so it is safe from a
/// crash handler, which means "the registry was busy" and "no name covers
/// this address" are different answers that must not be conflated — the
/// distinction `JitNameLookup` exists for. Same convention as
/// `live_code_region_covers_a_buffer_the_name_registry_never_saw`.
fn name_of(addr: usize) -> Option<String> {
    for _ in 0..200 {
        match lookup_jit_method_name_detailed(addr) {
            JitNameLookup::Found(n) => return Some(n),
            JitNameLookup::NotFound => return None,
            JitNameLookup::Locked => std::thread::yield_now(),
        }
    }
    panic!("the name registry stayed locked for 200 attempts");
}

/// THE redefinition hole, from the install side.
///
/// `redefineClass` bumps the epoch and flushes the cache, but the
/// compilation broker stamps nothing on a queued or in-flight request, so a
/// compile that already read the OLD bytecode completes afterwards. Before
/// the install barrier, `put` published it unconditionally and the VM then
/// executed the bytecode the agent had replaced.
#[test]
fn a_compilation_that_predates_a_cache_flush_is_not_installed() {
    let cache = JitCache::new();
    let (class, method, desc, cid) = key();

    // A compilation opens, reads bytecode, and produces an artifact...
    let witness = open_compile_epoch_witness();
    let stale = ret_body();

    // ...and a redefinition flushes the cache while it is still in flight.
    cache.clear_all();
    drop(witness);

    cache.put(class.clone(), method.clone(), desc.clone(), cid, stale);
    assert!(
        cache.get(&class, &method, &desc, cid).is_none(),
        "a body compiled against bytecode the flush retired must not be published"
    );
}

/// The gate must refuse only what it is meant to: a compilation that STARTS
/// after the flush publishes normally. Without this the previous test would
/// also pass on a `put` that never publishes anything.
#[test]
fn a_compilation_that_starts_after_a_flush_installs_normally() {
    let cache = JitCache::new();
    let (class, method, desc, cid) = key();

    cache.clear_all();
    let witness = open_compile_epoch_witness();
    let fresh = ret_body();
    drop(witness);

    cache.put(class.clone(), method.clone(), desc.clone(), cid, fresh);
    assert!(
        cache.get(&class, &method, &desc, cid).is_some(),
        "a body compiled after the flush is current and must publish"
    );
}

/// The stamp has to be the epoch at compile START, not at buffer finalize —
/// the whole window the broker cannot see is the one *before* codegen ends.
///
/// Race-free by construction: both artifacts are compared against each
/// other rather than against a separately-read global, so a concurrent bump
/// from another test cannot change the outcome.
#[test]
fn the_compile_witness_stamps_the_start_epoch_not_the_finalize_epoch() {
    let witness = open_compile_epoch_witness();
    let at_start = ret_body().install_epoch;
    bump_jit_install_epoch();
    bump_jit_install_epoch();
    let at_finalize = ret_body().install_epoch;
    drop(witness);

    assert_eq!(
        at_start, at_finalize,
        "an artifact finalized after a mid-compile epoch bump must still \
         carry the epoch its compilation began at"
    );

    // And with no witness open, the live epoch is used — otherwise a
    // backend entered directly would inherit a stale thread-local.
    let unscoped = ret_body().install_epoch;
    assert!(
        unscoped > at_start,
        "outside a compilation scope the stamp must track the live epoch \
         (got {unscoped}, compile-scope stamp was {at_start})"
    );
}

/// `callee_compiler` re-enters `try_compile` on the same thread. The nested
/// compile must keep the OUTER (older) epoch: it is only useful if the outer
/// artifact is, and an older stamp can only refuse more.
#[test]
fn a_nested_compile_keeps_the_outer_epoch() {
    let outer = open_compile_epoch_witness();
    let outer_stamp = ret_body().install_epoch;
    {
        let inner = open_compile_epoch_witness();
        bump_jit_install_epoch();
        assert_eq!(
            ret_body().install_epoch,
            outer_stamp,
            "a nested compile must not adopt a newer epoch than its caller"
        );
        drop(inner);
    }
    assert_eq!(
        ret_body().install_epoch,
        outer_stamp,
        "closing the nested scope must restore the outer compilation's epoch"
    );
    drop(outer);
}

/// The barrier is per-cache on purpose. A global watermark would make one
/// VM's redefinition refuse another VM's unrelated publication — and, in
/// this test binary, would make every `clear_all` test a random failure
/// generator for every concurrent `put` test.
#[test]
fn flushing_one_cache_does_not_refuse_another_caches_publication() {
    let flushed = JitCache::new();
    let untouched = JitCache::new();
    let (class, method, desc, cid) = key();

    let witness = open_compile_epoch_witness();
    let body = ret_body();
    flushed.clear_all();
    drop(witness);

    untouched.put(class.clone(), method.clone(), desc.clone(), cid, body);
    assert!(
        untouched.get(&class, &method, &desc, cid).is_some(),
        "a cache that was never flushed must not inherit another cache's barrier"
    );
}

/// `JitPICSlot::seed_from_mic` used to read the MIC's entry pointer BEFORE
/// its owner. `JitMICSlot::clear_compiled_entry` zeroes the entry and only
/// then takes the owner, so the old order could copy a live raw address into
/// the PIC together with an owner that had already been released — a
/// callable pointer with no keep-alive, which the emitted 4-way cascade
/// `CALL`s. This is that state, written directly.
#[test]
fn seeding_a_pic_refuses_a_mic_entry_that_lost_its_owner() {
    use std::sync::atomic::Ordering;

    // A live JIT code region: an unowned address inside one is a compiled
    // body we failed to retain, never a native trampoline.
    let buf = ExecutableBuffer::new(64).expect("alloc executable");
    let orphan = buf.as_ptr() as u64;

    let mic = JitMICSlot::new();
    mic.cached_entry_word
        .store(orphan | JIT_IC_NEEDS_CONTEXT_TAG, Ordering::Release);
    mic.cached_class_id.store(7, Ordering::Release);
    // `compiled_owner` deliberately left empty — the residue of the race.

    let pic = JitPICSlot::new();
    pic.seed_from_mic(&mic, false);

    assert_eq!(
        pic.class_ids[0].load(Ordering::Acquire),
        7,
        "the class guard is still worth carrying forward"
    );
    assert_eq!(
        pic.entry_words[0].load(Ordering::Acquire),
        0,
        "an entry with no live owner must be downgraded to unresolved, not \
         copied into a slot generated code calls without validation — and \
         the ABI flag travels in that word, so it goes with it"
    );
    drop(buf);
}

/// An admissible MIC entry still seeds through — the refusal above must not
/// be "seed_from_mic no longer copies anything".
#[test]
fn seeding_a_pic_carries_a_well_formed_mic_entry_forward() {
    use std::sync::atomic::Ordering;

    let mic = JitMICSlot::new();
    // A synthetic non-JIT sentinel: unowned but outside every code region,
    // which is the shape `jit_entry_publishable` admits (native/builtin
    // targets, which nothing unmaps). Same value the sibling PIC tests in
    // `mod tests` use, so it is empirically clear of every real mapping this
    // suite makes.
    let sentinel = 0x7000u64;
    mic.cached_entry_word.store(sentinel, Ordering::Release);
    mic.cached_class_id.store(9, Ordering::Release);

    let pic = JitPICSlot::new();
    pic.seed_from_mic(&mic, false);

    assert_eq!(pic.class_ids[0].load(Ordering::Acquire), 9);
    assert_eq!(pic.entry_words[0].load(Ordering::Acquire), sentinel);
}

/// A retired body must stop naming its address. The OS is free to hand that
/// page to the next `alloc_executable`, and a crash report that names the
/// dead method is worse than one that names none — it is believed.
#[test]
fn a_retired_body_withdraws_its_crash_report_name() {
    let cm = ret_body();
    let entry = cm.entry_ptr() as usize;
    let len = cm.code_len().max(1);
    let name = "cclt/Retired.body()V".to_string();
    register_jit_method_name(entry, len, name.clone());
    assert_eq!(
        name_of(entry).as_deref(),
        Some(name.as_str()),
        "the name must resolve while the body is live"
    );

    drop(cm);
    // `!= our name` rather than `is_none`: the allocator may already have
    // handed this page to a concurrently-running test, which would then
    // legitimately own the address. What must never happen is the DEAD
    // body still answering for it.
    assert_ne!(
        name_of(entry).as_deref(),
        Some(name.as_str()),
        "a dropped body must not keep symbolizing an address the allocator \
         can hand to a different body"
    );
}

/// Belt to the withdrawal's braces: if a registration is ever missed, the
/// newest range for an address must still win, so a recycled address
/// symbolizes as what is mapped there NOW.
#[test]
// x86-64 only: it registers x86-64 compiled bodies by address; the aarch64
// path publishes different artifacts. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn the_newest_registration_for_an_address_wins() {
    let buf = ExecutableBuffer::new(64).expect("alloc executable");
    let entry = buf.as_ptr() as usize;
    register_jit_method_name(entry, 16, "cclt/First.old()V".to_string());
    register_jit_method_name(entry, 16, "cclt/Second.new()V".to_string());
    assert_eq!(
        name_of(entry).as_deref(),
        Some("cclt/Second.new()V"),
        "the most recent publication for an address must be the one reported"
    );
    unregister_jit_method_name(entry);
    assert_ne!(
        name_of(entry).as_deref(),
        Some("cclt/Second.new()V"),
        "withdrawal must remove EVERY range starting at the entry, not just one"
    );
    assert_ne!(
        name_of(entry).as_deref(),
        Some("cclt/First.old()V"),
        "and it must remove the older range for the same entry too"
    );
    drop(buf);
}

/// `lookup_jit_code_range` hands out a bare `Arc<CompiledMethod>` inner
/// address with no keep-alive; the pinning form must hand out a reference
/// that makes the metadata (and the code buffer) valid for as long as it is
/// held, even after the cache has released the body.
#[test]
fn pinning_a_code_range_owner_retains_the_artifact() {
    let cache = JitCache::new();
    let (class, method, desc, cid) = key();
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    let published = cache.get(&class, &method, &desc, cid).expect("published");
    let entry = published.entry_ptr() as usize;
    register_jit_code_range(
        entry,
        published.code_len().max(1),
        Arc::as_ptr(&published) as usize,
    );

    let pinned = pin_jit_code_range_owner(entry).expect("a live body must pin");
    assert_eq!(pinned.entry_ptr() as usize, entry);
    // A pin is a real strong reference: it survives every other holder
    // releasing, which is exactly what the bare-address form cannot promise.
    drop(published);
    cache.clear_all();
    assert_eq!(
        pinned.entry_ptr() as usize,
        entry,
        "the pinned artifact must stay valid after the cache released it"
    );
    assert_eq!(
        pinned.code_bytes().first().copied(),
        Some(0xC3),
        "and its code buffer must still be mapped and readable"
    );
    drop(pinned);
    unregister_jit_code_range(entry);
}

/// A registered range whose body was never published — so nothing in
/// `jit_entry_owners` can retain it — must pin as `None`. `None` is the safe
/// answer; a raw address would not be.
#[test]
fn pinning_an_unowned_code_range_yields_none() {
    let cm = ret_body();
    let entry = cm.entry_ptr() as usize;
    register_jit_code_range(entry, cm.code_len().max(1), &cm as *const _ as usize);
    assert!(
        pin_jit_code_range_owner(entry).is_none(),
        "a body with no retainable owner must not be handed out as pinned"
    );
    drop(cm);
}

/// An address no body covers pins nothing.
#[test]
fn pinning_an_unmapped_address_yields_none() {
    assert!(pin_jit_code_range_owner(0).is_none());
    assert!(pin_jit_code_range_owner(0x10).is_none());
}

/// The bisect levers' matching rule. Pinned as a pure function because the
/// live predicate reads two `OnceLock`s seeded from the environment once
/// per process, which no test can set reproducibly.
///
/// The load-bearing case is the last one. `CRATONVM_DBG=jit-bisect-only`
/// with a prefix matching NOTHING must force EVERY method interpreted —
/// that is what makes "allow only X, does the crash survive?" a valid
/// bisect step. See the retired `annotation-scan-arrayread-sigsegv`
/// write-up: the OSR path did not consult this predicate at all, so 21 OSR bodies
/// compiled under exactly that setting and every bisect row read a
/// meaningless "no effect".
#[test]
fn bisect_levers_match_deny_by_substring_and_allow_only_by_prefix() {
    let deny = vec!["org/h2/mvstore/MVStore.commit".to_string()];
    assert!(force_interpret_matches(
        Some(&deny),
        None,
        "org/h2/mvstore/MVStore",
        "commit"
    ));
    assert!(!force_interpret_matches(
        Some(&deny),
        None,
        "org/h2/mvstore/MVStore",
        "rollback"
    ));

    // A bare package entry pins everything under it.
    let pkg = vec!["org/keycloak/".to_string()];
    assert!(force_interpret_matches(
        Some(&pkg),
        None,
        "org/keycloak/Foo",
        "bar"
    ));

    // Allowlist: listed prefix stays eligible, everything else does not.
    let only = vec!["org/apache/tomcat".to_string()];
    assert!(!force_interpret_matches(
        None,
        Some(&only),
        "org/apache/tomcat/util/bcel/classfile/ConstantPool",
        "getConstant"
    ));
    assert!(force_interpret_matches(
        None,
        Some(&only),
        "java/io/BufferedInputStream",
        "read"
    ));

    // Neither lever set: nothing is forced interpreted.
    assert!(!force_interpret_matches(None, None, "any/Class", "any"));

    // A prefix that matches nothing forces EVERY method interpreted.
    let nothing = vec!["zzzNoSuchPrefix".to_string()];
    for (c, m) in [
        ("java/io/BufferedInputStream", "read"),
        (
            "org/apache/tomcat/util/bcel/classfile/ConstantPool",
            "getConstant",
        ),
        ("AnnotationScanSplitProbe", "readBytes"),
    ] {
        assert!(
            force_interpret_matches(None, Some(&nothing), c, m),
            "bisect-only with an unmatched prefix must force {c}.{m} interpreted"
        );
    }
}
