use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Publish `versions` distinct bodies for each of `classes` receivers and
/// return `(class_id, version) -> entry` plus `entry -> class_id`.
fn published_bodies(
    cache: &JitCache,
    classes: u32,
    versions: u32,
) -> (
    std::collections::HashMap<(u32, u32), u64>,
    std::collections::HashMap<u64, u32>,
) {
    let mut by_key = std::collections::HashMap::new();
    let mut class_of = std::collections::HashMap::new();
    for class_id in 1..=classes {
        for version in 0..versions {
            let class: Arc<str> = Arc::from(format!("icpub/C{class_id}").as_str());
            let method: Arc<str> = Arc::from(format!("m{version}").as_str());
            let desc: Arc<str> = Arc::from("()V");
            let cid = cratonvm_types::ClassId::new(class_id);
            let mut buf = ExecutableBuffer::new(64).expect("alloc body");
            buf.emit(&[0xC3]);
            cache.put(
                class.clone(),
                method.clone(),
                desc.clone(),
                cid,
                CompiledMethod::new(buf),
            );
            let cm = cache.get(&class, &method, &desc, cid).expect("published");
            let entry = cm.entry_ptr() as u64;
            by_key.insert((class_id, version), entry);
            class_of.insert(entry, class_id);
        }
    }
    (by_key, class_of)
}

/// Threads missing on the same cold site with DIFFERENT receivers used to
/// write the same empty way unreserved, leaving (class A, entry of B) and an
/// owner that disagreed with the entry. Every way that ends up live must
/// name one class, that class's entry, and that entry's owner.
#[test]
fn concurrent_installs_of_different_receivers_never_mix_a_way() {
    let cache = JitCache::new();
    let (by_key, class_of) = published_bodies(&cache, 4, 1);
    for _round in 0..64 {
        let pic = Arc::new(JitPICSlot::new());
        let start = Arc::new(std::sync::Barrier::new(4));
        let workers: Vec<_> = (1..=4u32)
            .map(|class_id| {
                let pic = pic.clone();
                let start = start.clone();
                let entry = by_key[&(class_id, 0)];
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..16 {
                        pic.install(class_id, "icpub", entry, class_id % 2 == 0, false);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("installer finishes");
        }
        for class_id in 1..=4u32 {
            let (entry, needs_context) =
                pic.lookup(class_id).expect("four receivers fit four ways");
            assert_eq!(
                class_of[&entry], class_id,
                "lookup({class_id}) returned another class's entry"
            );
            assert_eq!(needs_context, class_id % 2 == 0);
        }
        for i in 0..JIT_PIC_ENTRIES {
            let (class_id, entry, _) = pic.way(i).expect("way exists");
            if !JitPICSlot::is_live_class_id(class_id) {
                continue;
            }
            assert_eq!(
                class_of[&entry], class_id,
                "way {i} pairs a guard with another class's entry"
            );
            let owner = pic.compiled_owners[i]
                .lock()
                .as_ref()
                .map(|o| o.entry_ptr() as u64);
            assert_eq!(
                owner,
                Some(entry),
                "way {i}'s owner must be the body its word names"
            );
        }
    }
}

/// Readers that follow generated code's load order — class id, then one
/// entry-word load, all inside one JIT execution — racing installs of
/// recompiled targets and `clear_entries`. A reader that matched class C
/// must never load a word naming a body of any other class: that is the
/// retarget-under-a-reader the write-once protocol exists to prevent, and
/// the grace period is what keeps a retired way from being reused under a
/// reader still between its two loads.
#[test]
fn readers_never_pair_a_guard_with_another_receivers_entry() {
    let cache = JitCache::new();
    let (by_key, class_of) = published_bodies(&cache, 6, 3);
    let class_of = Arc::new(class_of);
    let pic = Arc::new(JitPICSlot::new());
    let stop = Arc::new(AtomicBool::new(false));
    // Published so the WRITER can see whether the readers have got any
    // work yet. See the round loop below for why a fixed round count is
    // not enough on a loaded machine.
    let observed = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let pic = pic.clone();
            let stop = stop.clone();
            let class_of = class_of.clone();
            let observed = observed.clone();
            std::thread::spawn(move || {
                let mut checked = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let execution = jit_execution_enter();
                    for i in 0..JIT_PIC_ENTRIES {
                        let class_id = pic.class_ids[i].load(Ordering::Acquire);
                        if !JitPICSlot::is_live_class_id(class_id) {
                            continue;
                        }
                        // Widen the window between the two loads.
                        std::hint::spin_loop();
                        let (entry, _) = jit_ic_entry_decode(pic.entry_words[i].load(Ordering::Acquire));
                        if entry == 0 {
                            continue;
                        }
                        assert_eq!(
                            class_of.get(&entry).copied(),
                            Some(class_id),
                            "a reader past way {i}'s guard for class {class_id} loaded another receiver's entry"
                        );
                        checked += 1;
                        observed.fetch_add(1, Ordering::Relaxed);
                    }
                    jit_execution_leave(execution);
                }
                checked
            })
        })
        .collect();

    // The writer used to run a FIXED 2,000 rounds. On a machine under load
    // -- a full-workspace `cargo test`, or sixteen copies of this binary at
    // `--test-threads 16`, which is how this surfaced -- all 2,000 can
    // complete before any of the four reader threads is scheduled long
    // enough to see a non-zero entry word. `checked` is then 0, and the
    // assertion below correctly refuses to report `ok` for a run that
    // proved nothing. That is the right verdict and the wrong outcome: it
    // made a genuine race-detection test read as a red build roughly one
    // run in five under contention (measured: 10 of 48).
    //
    // So the writer now runs until the readers have actually observed a
    // published way, with a hard cap. The cap is what keeps this a test:
    // if the protocol ever genuinely starves readers, the loop still ends
    // and the assertion still fires.
    const MIN_ROUNDS: u32 = 2_000;
    // The cap is a wall-clock deadline, not a round count: on a host at load
    // 15-30 all 200,000 rounds finish inside one scheduler slice of the
    // starved readers (this test failed on the Linux host's `cratonvm-jit
    // --lib` run). Once the minimum is done the writer yields the CPU each
    // round so the readers get scheduled, and stops at the first observed
    // way or after 30 s.
    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
    let started = std::time::Instant::now();
    let mut round = 0u32;
    while round < MIN_ROUNDS
        || (observed.load(Ordering::Relaxed) == 0 && started.elapsed() < MAX_WAIT)
    {
        if round >= MIN_ROUNDS {
            std::thread::yield_now();
        }
        let class_id = 1 + round % 6;
        let version = round % 3;
        pic.install(
            class_id,
            "icpub",
            by_key[&(class_id, version)],
            false,
            false,
        );
        if round % 97 == 0 {
            pic.clear_entries();
        }
        round += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let checked: u64 = readers
        .into_iter()
        .map(|r| r.join().expect("reader finishes"))
        .sum();
    assert!(
        checked > 0,
        "the readers must actually have raced a published way"
    );
}

/// An invalidation marks a body retired and then clears the inline caches;
/// a helper that resolved the body just before must not leave it installed.
/// Either the install sees the flag (admission or the post-publication
/// re-check), or the clearing pass — which takes the same writer lock —
/// runs after the publication and withdraws it.
#[test]
fn a_retired_body_never_survives_a_racing_install() {
    let cache = JitCache::new();
    let (by_key, _) = published_bodies(&cache, 1, 1);
    let entry = by_key[&(1, 0)];
    let owner = resolve_jit_entry_owner(entry as usize).expect("published body is owned");

    for _round in 0..200 {
        owner.retired.store(false, Ordering::SeqCst);
        let pic = Arc::new(JitPICSlot::new());
        let mic = Arc::new(JitMICSlot::new());
        let installer = {
            let (pic, mic) = (pic.clone(), mic.clone());
            std::thread::spawn(move || {
                for _ in 0..8 {
                    mic.update(1, "icpub", entry, false, false);
                    pic.install(1, "icpub", entry, false, false);
                }
            })
        };
        // The invalidation's order: retire first, then clear.
        owner.retired.store(true, Ordering::SeqCst);
        let targets: std::collections::HashSet<usize> = [entry as usize].into_iter().collect();
        pic.invalidate_targets(&targets);
        mic.invalidate_target(&targets);
        installer.join().expect("installer finishes");

        assert!(
            pic.lookup(1).map_or(true, |(e, _)| e != entry),
            "a retired body stayed in a PIC way"
        );
        assert!(
            pic.lookup_megamorphic(1).is_none(),
            "a retired body stayed in the hashed table"
        );
        assert_ne!(
            mic.cached_entry().0,
            entry,
            "a retired body stayed in the MIC"
        );
    }
    owner.retired.store(false, Ordering::SeqCst);
}

/// The deterministic half: a body already retired is never published.
#[test]
fn installing_an_already_retired_body_publishes_nothing() {
    let cache = JitCache::new();
    let (by_key, _) = published_bodies(&cache, 1, 1);
    let entry = by_key[&(1, 0)];
    let owner = resolve_jit_entry_owner(entry as usize).expect("published body is owned");
    owner.retired.store(true, Ordering::SeqCst);

    let mic = JitMICSlot::new();
    mic.update(1, "icpub", entry, true, false);
    assert_eq!(mic.cached_entry(), (0, false));

    let pic = JitPICSlot::new();
    pic.install(1, "icpub", entry, true, false);
    assert_eq!(pic.entries_used(), 0);
    assert!(pic.lookup_megamorphic(1).is_none());
    owner.retired.store(false, Ordering::SeqCst);
}
