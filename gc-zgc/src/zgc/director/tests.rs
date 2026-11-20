use super::*;

const M: u64 = 1024 * 1024;

fn base() -> DirectorInputs {
    DirectorInputs {
        soft_max: 1024 * M,
        used: 0,
        used_after_last_cycle: 0,
        rate_avg: 0.0,
        rate_sd: 0.0,
        rate_known: true,
        cycle_avg: 0.05,
        cycle_sd: 0.0,
        cycles_timed: 1,
        cycles: 1,
        since_last_cycle: 0.0,
        stw_model: false,
        poll_latency_bytes: poll_latency_bytes(1024 * M),
        byte_polled_interval: false,
        hotspot_warmth: false,
        warmup_cycles: 0,
        proactive_off: false,
        spike_tolerance: None,
        proactive_major_clock: false,
        since_last_major: 0.0,
        used_after_last_major: 0,
    }
}

/// zt t6/tc (zt1tp item 4): `-XX:-ZProactive` silences the proactive
/// rule on inputs where it fires, and a higher
/// `-XX:ZAllocationSpikeTolerance` reserves more (fires earlier). The
/// `ZDirector` setters reach the inputs.
#[test]
fn zt6tc_zproactive_and_spike_tolerance_reach_the_rules() {
    let mut i = base();
    i.cycles = WARM_CYCLES;
    i.used_after_last_cycle = 10 * M;
    i.used = 10 * M + 200 * M;
    i.since_last_cycle = 600.0;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::Proactive));
    i.proactive_off = true;
    assert_ne!(evaluate(&i).fire_now, Some(DirectorRule::Proactive));

    let mut r = base();
    r.rate_avg = 100.0 * M as f64;
    let default_at = evaluate(&r).fire_at_used;
    r.spike_tolerance = Some(4.0);
    let tolerant_at = evaluate(&r).fire_at_used;
    assert!(
        tolerant_at < default_at,
        "a higher tolerance fires earlier: {tolerant_at} vs {default_at}"
    );

    let d = ZDirector::new(1024 * M, true, true);
    assert_eq!(d.spike_tolerance(), None);
    d.set_allocation_spike_tolerance(5.0);
    assert_eq!(d.spike_tolerance(), Some(5.0));
    d.set_allocation_spike_tolerance(f64::NAN);
    assert_eq!(d.spike_tolerance(), None, "a bad value keeps the default");
    d.set_proactive(false);
    assert!(d.proactive_off.load(Ordering::Relaxed));
}

/// zt t7/x4: HotSpot 25.0.3 keeps `-XX:ZAllocationSpikeTolerance=0` and
/// `=-1` (no range; `{command line}` under `-XX:+PrintFlagsFinal`), so the
/// director keeps them too. `0` used to be the "unset" word, so a `0`
/// tolerance read as the default 2.0. A zero tolerance reserves only the
/// deviation term (fires no earlier than the default); a negative one is
/// HotSpot's own arithmetic and must not panic. Control: NaN and infinity
/// still keep the default.
#[test]
fn zt7x4_a_zero_or_negative_spike_tolerance_is_kept() {
    let d = ZDirector::new(1024 * M, true, true);
    d.set_allocation_spike_tolerance(0.0);
    assert_eq!(d.spike_tolerance(), Some(0.0), "zero is a value, not unset");
    d.set_allocation_spike_tolerance(-1.0);
    assert_eq!(d.spike_tolerance(), Some(-1.0));
    d.set_allocation_spike_tolerance(f64::INFINITY);
    assert_eq!(
        d.spike_tolerance(),
        None,
        "control: infinity keeps the default"
    );

    let mut r = base();
    r.rate_avg = 100.0 * M as f64;
    let default_at = evaluate(&r).fire_at_used;
    r.spike_tolerance = Some(0.0);
    let zero_at = evaluate(&r).fire_at_used;
    assert!(
        zero_at >= default_at,
        "a zero tolerance reserves less: {zero_at} vs {default_at}"
    );
    r.spike_tolerance = Some(-1.0);
    let _ = evaluate(&r);
}

/// zt t7/tg: the idle tick's question. A warm heap that stopped
/// allocating gets a proactive yes once [`PROACTIVE_MAX_INTERVAL`] passed
/// since the last cycle (HotSpot's `rule_major_proactive` stale arm), not
/// before; `-XX:-ZProactive` and a non-deciding (shadow) director say no.
#[test]
fn zt7tg_an_idle_warm_heap_gets_a_proactive_yes_after_five_minutes() {
    let warm = |deciding: bool| {
        let d = ZDirector::new(1024 * M, deciding, true);
        let t0 = Instant::now();
        for i in 0..WARM_CYCLES {
            d.note_cycle_end(
                t0 + Duration::from_millis(10 * i),
                Some(Duration::from_millis(5)),
                10 * M,
            );
        }
        (d, t0 + Duration::from_millis(10 * (WARM_CYCLES - 1)))
    };
    let (d, last) = warm(true);
    assert!(
        !d.idle_proactive_due(10 * M, last + Duration::from_secs(10)),
        "ten idle seconds are not stale"
    );
    let stale = last + PROACTIVE_MAX_INTERVAL + Duration::from_secs(1);
    assert!(d.idle_proactive_due(10 * M, stale));
    d.set_proactive(false);
    assert!(!d.idle_proactive_due(10 * M, stale), "-XX:-ZProactive");
    let (shadow, last) = warm(false);
    let late = last + PROACTIVE_MAX_INTERVAL + Duration::from_secs(1);
    assert!(
        !shadow.idle_proactive_due(10 * M, late),
        "a shadow director never asks"
    );
}

#[test]
fn decaying_seq_weights_the_new_value_thirty_percent() {
    let mut s = DecayingSeq::default();
    s.add(10.0);
    assert_eq!(s.davg(), 10.0);
    assert_eq!(s.dsd(), 0.0);
    s.add(20.0);
    assert!((s.davg() - 13.0).abs() < 1e-9);
    // diff = 20 - 13 = 7; dvar = 0.3 * 49
    assert!((s.dsd() - (0.3f64 * 49.0).sqrt()).abs() < 1e-9);
}

#[test]
fn window_seq_keeps_the_last_ten() {
    let mut w = WindowSeq::default();
    for _ in 0..10 {
        w.add(100.0);
    }
    assert_eq!(w.avg(), 100.0);
    assert_eq!(w.sd(), 0.0);
    for _ in 0..10 {
        w.add(200.0);
    }
    assert_eq!(w.len(), 10);
    assert_eq!(w.avg(), 200.0);
    let mut w = WindowSeq::default();
    w.add(1.0);
    w.add(3.0);
    assert_eq!(w.avg(), 2.0);
    assert_eq!(w.sd(), 1.0);
}

/// The rate rule fires exactly when free space no longer covers the
/// worst-case rate over one cycle plus one sample interval.
#[test]
fn the_rate_rule_fires_when_time_to_exhaustion_is_shorter_than_a_cycle() {
    // 500 MiB/s average, no variance: worst case 1000 MiB/s. Cycle 50 ms
    // + interval 100 ms = 0.15 s -> reserve ~150 MiB, i.e. earlier than
    // the high-usage backstop (5% of 1 GiB = ~51 MiB free).
    let mut i = base();
    i.rate_avg = 500.0 * M as f64;
    i.used = 512 * M;
    let d = evaluate(&i);
    assert_eq!(d.fire_now, None);
    assert_eq!(d.fire_at_rule, Some(DirectorRule::AllocationRate));
    let reserve = 1024 * M - d.fire_at_used;
    assert!(reserve > 149 * M && reserve < 151 * M, "reserve {reserve}");
    // At the published threshold the same evaluation says yes.
    i.used = d.fire_at_used;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::AllocationRate));
    // One byte-step under it, no.
    i.used = d.fire_at_used - 4 * M;
    assert_eq!(evaluate(&i).fire_now, None);
}

/// Variance moves the trigger earlier: the same mean with a noisy window
/// reserves more.
#[test]
fn rate_variance_widens_the_reserve() {
    let mut quiet = base();
    quiet.rate_avg = 100.0 * M as f64;
    let mut noisy = quiet;
    noisy.rate_sd = 50.0 * M as f64;
    assert!(evaluate(&noisy).fire_at_used < evaluate(&quiet).fire_at_used);
    let mut slow_cycles = quiet;
    slow_cycles.cycle_sd = 0.05;
    assert!(evaluate(&slow_cycles).fire_at_used < evaluate(&quiet).fire_at_used);
}

/// Before a cycle has been timed the rate rule is off; high usage still
/// guards the top of the heap.
#[test]
fn untimed_heap_has_only_the_high_usage_backstop() {
    let mut i = base();
    i.cycles_timed = 0;
    i.rate_avg = 10_000.0 * M as f64;
    i.used = 900 * M;
    let d = evaluate(&i);
    assert_eq!(d.fire_now, None);
    assert_eq!(d.time_until_gc, f64::INFINITY);
    assert_eq!(d.fire_at_rule, Some(DirectorRule::HighUsage));
    i.used = 1000 * M;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::HighUsage));
}

#[test]
fn proactive_needs_warmth_growth_and_an_affordable_interval() {
    let mut i = base();
    i.cycles = WARM_CYCLES;
    i.used_after_last_cycle = 100 * M;
    i.used = 100 * M + 103 * M; // > 10% of 1 GiB
    i.cycle_avg = 0.01; // acceptable interval = 0.49 s
    i.since_last_cycle = 0.2;
    assert_eq!(evaluate(&i).fire_now, None, "interval too short");
    i.since_last_cycle = 0.5;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::Proactive));
    let mut cold = i;
    cold.cycles = WARM_CYCLES - 1;
    assert_eq!(evaluate(&cold).fire_now, None, "not warm");
    let mut flat = i;
    flat.used = 150 * M;
    assert_eq!(evaluate(&flat).fire_now, None, "no growth, not stale");
    flat.since_last_cycle = 301.0;
    assert_eq!(evaluate(&flat).fire_now, Some(DirectorRule::Proactive));
}

/// zt t14/tg6 (t12 verdicts new defect 5, `CRATONVM_ZGC_PROACTIVE_MAJOR_CLOCK`):
/// under the major clock the proactive rule reads the time and the base
/// of the last major, so a recent minor-capable cycle (here 0.01 s ago,
/// at a high base) does not hold it back; kill control: the same inputs
/// on every cycle's clock say no. The `ZDirector` arm: only
/// `note_major_cycle_end` moves the major clock.
#[test]
fn zt14tg6_the_proactive_rule_reads_the_major_clock() {
    let mut i = base();
    i.cycles = WARM_CYCLES;
    i.cycle_avg = 0.01; // acceptable interval = 0.49 s
    i.used_after_last_cycle = 300 * M;
    i.since_last_cycle = 0.01;
    i.used_after_last_major = 100 * M;
    i.since_last_major = 2.0;
    i.used = 310 * M; // > 10% of 1 GiB above the major's base only
    assert_eq!(evaluate(&i).fire_now, None, "kill: every cycle's clock");
    i.proactive_major_clock = true;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::Proactive));
    i.since_last_major = 0.2;
    assert_eq!(evaluate(&i).fire_now, None, "the major is too recent");

    let d = ZDirector::new(1024 * M, true, true);
    let t0 = Instant::now();
    let base_major = d.state.lock().last_major_end;
    d.note_cycle_end(t0 + Duration::from_secs(1), None, 7 * M);
    assert_eq!(d.state.lock().last_major_end, base_major, "a cycle alone");
    d.note_major_cycle_end(t0 + Duration::from_secs(2), 9 * M);
    let st = d.state.lock();
    assert_eq!(st.last_major_end, t0 + Duration::from_secs(2));
    assert_eq!(st.used_after_last_major, 9 * M);
}

/// zt t20/tg9 (Defect D8, row `tg6_p1`): under the major clock a due
/// proactive cycle wins over an allocation-rate yes, as HotSpot asks its
/// major rules before its minor ones; the kill keeps the rate-first order
/// (negative control), and with nothing due the rate rule still fires
/// (positive control that the inputs say yes to it).
#[test]
fn zt20tg9_a_due_proactive_cycle_wins_over_the_rate_rule_under_the_major_clock() {
    let mut i = base();
    i.cycles = WARM_CYCLES;
    i.cycle_avg = 0.01; // acceptable interval = 0.49 s
    i.rate_avg = 10_000.0 * M as f64; // reserve beyond the soft maximum
    i.used_after_last_cycle = 65 * M;
    i.since_last_cycle = 0.02;
    i.used_after_last_major = 65 * M;
    i.since_last_major = 2.0;
    i.used = 200 * M; // > 10% of 1 GiB above the major's base
    assert_eq!(
        evaluate(&i).fire_now,
        Some(DirectorRule::AllocationRate),
        "kill: rate first"
    );
    i.proactive_major_clock = true;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::Proactive));
    i.since_last_major = 0.2;
    assert_eq!(
        evaluate(&i).fire_now,
        Some(DirectorRule::AllocationRate),
        "nothing due: the rate rule"
    );
}

#[test]
fn hotspot_cause_spellings() {
    assert_eq!(
        DirectorRule::AllocationRate.hotspot_cause(),
        "Allocation Rate"
    );
    assert_eq!(DirectorRule::Proactive.hotspot_cause(), "Proactive");
    assert_eq!(DirectorRule::HighUsage.hotspot_cause(), "High Usage");
}

/// The poll publishes a byte threshold and fires on it without the clock;
/// a cycle end re-arms. Positive control for the deciding arm and the
/// shadow record, and the off arm stays silent.
#[test]
fn poll_fires_on_the_published_threshold_and_cycle_end_rearms() {
    let d = ZDirector::new(1024 * M, true, false);
    let t0 = Instant::now();
    // One timed cycle so the rate rule is live.
    d.note_cycle_end(t0, Some(Duration::from_millis(50)), 0);
    // 500 MiB in 1 s -> one 500 MiB/s sample.
    d.sample(500 * M, t0 + Duration::from_secs(1));
    let (avg, _, cyc, _, last) = d.snapshot();
    // The window keeps the clock it opened with (the director's creation,
    // a few microseconds before `t0`), so the rate is 500 MiB/s to 0.1 %.
    assert!((avg / (500.0 * M as f64) - 1.0).abs() < 1e-3, "{avg}");
    assert!((cyc - 0.05).abs() < 1e-9);
    let last = last.unwrap();
    assert_eq!(last.fire_at_rule, Some(DirectorRule::AllocationRate));
    let fire_at = last.fire_at_used;
    assert!(fire_at > 800 * M && fire_at < 1024 * M, "{fire_at}");
    // The threshold is now published: under it no, at it yes, with no
    // further sample (the next byte step is beyond `fire_at` here, so
    // force it out of reach).
    d.next_sample_used.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(d.poll(fire_at - 1), None);
    assert_eq!(d.poll(fire_at), Some(DirectorRule::AllocationRate));
    let shadow = d.note_cycle_end(t0 + Duration::from_secs(2), None, 10 * M);
    assert_eq!(shadow, Some((fire_at, DirectorRule::AllocationRate)));
    // Re-armed: nothing fires until a new sample.
    d.next_sample_used.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(d.poll(1000 * M), None);

    // Shadow-only: records, never decides.
    let s = ZDirector::new(1024 * M, false, true);
    s.note_cycle_end(t0, Some(Duration::from_millis(50)), 0);
    s.sample(500 * M, t0 + Duration::from_secs(1));
    let fire_at = s.snapshot().4.unwrap().fire_at_used;
    s.next_sample_used.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(s.poll(fire_at), None);
    assert_eq!(
        s.note_cycle_end(t0 + Duration::from_secs(2), None, 0),
        Some((fire_at, DirectorRule::AllocationRate))
    );

    // Off: inert.
    let off = ZDirector::new(1024 * M, false, false);
    assert_eq!(off.poll(1024 * M), None);
    assert_eq!(off.note_cycle_end(t0, None, 0), None);
}

/// zt t1/tp: HotSpot's warmth counts warm-up cycles only -- three
/// `System.gc()` cycles leave the rate rule off (no warm-up cycle yet,
/// times untrustable) and the proactive rule cold; the zr rule (`false`)
/// counted them.
#[test]
fn zt1tp_warmth_counts_warmup_cycles_as_hotspot_does() {
    let mut i = base();
    i.cycles = WARM_CYCLES;
    i.cycles_timed = WARM_CYCLES;
    i.rate_avg = 4096.0 * M as f64;
    i.used = 900 * M;
    assert_eq!(warmth(&i), (true, true), "the zr rule counts every cycle");
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::AllocationRate));
    i.hotspot_warmth = true;
    assert_eq!(warmth(&i), (false, false), "no warm-up cycle yet");
    let d = evaluate(&i);
    assert_eq!(d.fire_now, None);
    assert_eq!(d.time_until_gc, f64::INFINITY, "the rate rule is off");
    i.warmup_cycles = 1;
    assert_eq!(warmth(&i), (false, true));
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::AllocationRate));
    i.warmup_cycles = WARM_CYCLES;
    assert_eq!(warmth(&i), (true, true));
    // A measured duration is still needed.
    i.cycles_timed = 0;
    assert_eq!(warmth(&i), (false, false));

    // The director feeds the count from `note_warmup_cycle`.
    let d = ZDirector::new(1024 * M, true, false);
    d.set_hotspot_warmth(true);
    let t0 = Instant::now();
    d.note_cycle_end(t0, Some(Duration::from_millis(50)), 0);
    d.sample(800 * M, t0 + Duration::from_secs(1));
    assert_eq!(d.snapshot().4.unwrap().time_until_gc, f64::INFINITY);
    d.note_warmup_cycle();
    let t1 = t0 + Duration::from_secs(1);
    d.note_cycle_end(t1, Some(Duration::from_millis(50)), 0);
    d.sample(800 * M, t0 + Duration::from_secs(2));
    assert!(d.snapshot().4.unwrap().time_until_gc.is_finite());
}

/// zt t1/tp (zs1tp residuals item 3): the threshold and its rule are one
/// word, so a poll can never pair one decision's threshold with another's
/// rule; "never" and "already yes" survive the round trip.
#[test]
fn zt1tp_the_published_threshold_and_rule_are_one_word() {
    for code in 0..=3u64 {
        for used in [0, 1, 512 * M, FIRE_USED_NEVER - 1] {
            assert_eq!(unpack_fire(pack_fire(used, code)), (used, code));
        }
        assert_eq!(unpack_fire(pack_fire(u64::MAX, code)), (u64::MAX, code));
        assert_eq!(
            unpack_fire(pack_fire(FIRE_USED_NEVER, code)).0,
            u64::MAX,
            "at or above the field is never"
        );
    }
    let d = ZDirector::new(1024 * M, true, false);
    assert_eq!(d.fire_at_used(), u64::MAX, "nothing published");
    d.fire.store(
        pack_fire(700 * M, DirectorRule::HighUsage.code()),
        Ordering::Relaxed,
    );
    d.next_sample_used.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(d.fire_at_used(), 700 * M);
    assert_eq!(d.poll(700 * M - 1), None);
    assert_eq!(d.poll(700 * M), Some(DirectorRule::HighUsage));
    // The seam keeps the rule.
    d.dbg_set_fire_at_used(0);
    assert_eq!(d.poll(1), Some(DirectorRule::HighUsage));
    // A cycle end re-arms both halves at once.
    d.note_cycle_end(Instant::now(), None, 0);
    d.next_sample_used.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(d.fire_at_used(), u64::MAX);
    assert_eq!(d.poll(1023 * M), None);
}

#[test]
fn zr3cs_headroom_to_finish_a_concurrent_mark() {
    let mm = M as usize;
    let cap = 1024 * mm;
    let reserve = conc_urgent_reserve(cap, 8 * mm);
    assert_eq!(reserve, cap / 20);
    // Untimed: twice the reserve.
    assert!(conc_cycle_has_headroom(
        cap,
        cap - 2 * reserve - 1,
        reserve,
        0,
        0
    ));
    assert!(!conc_cycle_has_headroom(
        cap,
        cap - 2 * reserve,
        reserve,
        0,
        0
    ));
    // 1 byte/ns (2^20 scaled) for 100 ms -> 100 MB x 1.5 = 150 MB expected.
    let rate = 1u64 << 20;
    let ns = 100_000_000u64;
    assert_eq!(conc_expected_mark_allocation(rate, ns), Some(150_000_000));
    let need = reserve + 150_000_000;
    assert!(conc_cycle_has_headroom(
        cap,
        cap - need - 1,
        reserve,
        rate,
        ns
    ));
    assert!(!conc_cycle_has_headroom(cap, cap - need, reserve, rate, ns));
    // A small heap: the margin floors the reserve, a quarter caps it.
    assert_eq!(conc_urgent_reserve(64 * mm, 8 * mm), 8 * mm);
    assert_eq!(conc_urgent_reserve(16 * mm, 8 * mm), 4 * mm);
}

#[test]
fn zr3cs_the_gate_holds_until_the_mark_finishes_and_never_without_limit() {
    let mm = M as usize;
    let cap = 1024 * mm;
    let reserve = cap / 20;
    let mut i = ConcTriggerInputs {
        capacity: cap,
        reserve,
        used: 300 * mm,
        ..Default::default()
    };
    assert_eq!(conc_trigger_gate(&i), ConcTriggerGate::Clauses);
    assert_eq!(conc_trigger_gate(&i).answer(), None);
    // A request waits for its opener...
    i.requested_at = 300 * mm;
    assert_eq!(conc_trigger_gate(&i), ConcTriggerGate::Hold);
    assert_eq!(conc_trigger_gate(&i).answer(), Some(false));
    // ...for at most the grace...
    i.used = i.requested_at + conc_request_grace(reserve);
    assert_eq!(conc_trigger_gate(&i), ConcTriggerGate::RequestLapsed);
    // ...and not at all once the VM says the start was refused.
    i.used = i.requested_at;
    i.refused = true;
    assert_eq!(conc_trigger_gate(&i), ConcTriggerGate::RequestLapsed);
    i.refused = false;
    // Urgent space lapses it too.
    i.used = cap - reserve;
    assert_eq!(conc_trigger_gate(&i), ConcTriggerGate::RequestLapsed);
    // An open cycle holds while it traces, collects at its fixed point,
    // and is abandoned to a stop-the-world finish at the reserve.
    let mut o = ConcTriggerInputs {
        active: true,
        capacity: cap,
        reserve,
        used: 600 * mm,
        ..Default::default()
    };
    assert_eq!(conc_trigger_gate(&o), ConcTriggerGate::Hold);
    o.caught_up = true;
    assert_eq!(conc_trigger_gate(&o), ConcTriggerGate::MarkFinished);
    assert_eq!(conc_trigger_gate(&o).answer(), Some(true));
    o.caught_up = false;
    o.used = cap - reserve;
    assert_eq!(conc_trigger_gate(&o), ConcTriggerGate::UrgentWhileOpen);
}

/// The byte-polled reaction time: the cycle-duration term stays, HotSpot's
/// 100 ms tick term becomes the poll latency.
#[test]
fn the_byte_polled_interval_replaces_the_tick_term_and_keeps_the_duration() {
    let mut i = base();
    i.rate_avg = 2048.0 * M as f64; // worst case 4 GiB/s
    i.cycle_avg = 0.02;
    let tick = 1024 * M - evaluate(&i).fire_at_used;
    i.byte_polled_interval = true;
    let polled = 1024 * M - evaluate(&i).fire_at_used;
    // 4 GiB/s x 0.02 s ~ 82 MiB, plus the 8 MiB poll latency.
    assert!(polled > 85 * M && polled < 95 * M, "{polled}");
    assert!(polled * 4 < tick, "{polled} vs the tick-term reserve {tick}");
}

#[test]
fn sample_step_is_clamped() {
    assert_eq!(sample_step_bytes(0), 256 * 1024);
    assert_eq!(sample_step_bytes(1024 * M), 4 * M);
    assert_eq!(sample_step_bytes(64 * 1024 * M), 16 * M);
}

// ---- zr zs s1/tp ---------------------------------------------------------

/// The arithmetic behind the z1 +25 %: at a GB/s-class compiled allocation
/// rate the 0.1 s interval term dwarfs a STW cycle's duration; the STW
/// model reserves only the poll latency, which is bytes.
#[test]
fn zs1tp_the_interval_term_dominates_and_the_stw_model_reserves_the_poll_latency() {
    let mut i = base();
    i.rate_avg = 2048.0 * M as f64; // worst case 4 GiB/s
    i.cycle_avg = 0.02; // a 20 ms stop-the-world pause
    let hs = 1024 * M - evaluate(&i).fire_at_used;
    // HotSpot: 4 GiB/s x 0.12 s ~ 491 MiB.
    assert!(hs > 480 * M && hs < 500 * M, "{hs}");
    i.stw_model = true;
    assert_eq!(
        poll_latency_bytes(1024 * M),
        8 * M,
        "4 MiB step + 4 MiB consult"
    );
    let d = evaluate(&i);
    // At 1 GiB the 5% high-usage point (~51 MiB free) now comes first...
    assert_eq!(d.fire_at_rule, Some(DirectorRule::HighUsage));
    // ...and the rate rule itself says yes only with 8 MiB left.
    i.used = 1024 * M - 8 * M;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::AllocationRate));
    i.used -= 1;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::HighUsage));
    i.used = 0;
    // A small heap: the poll latency is above 5% and the rate rule binds.
    i.soft_max = 64 * M;
    i.poll_latency_bytes = poll_latency_bytes(64 * M);
    let small = evaluate(&i);
    assert_eq!(small.fire_at_rule, Some(DirectorRule::AllocationRate));
    assert_eq!(64 * M - small.fire_at_used, poll_latency_bytes(64 * M));
    // And it fires: free under the poll latency is "late".
    i.used = small.fire_at_used;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::AllocationRate));
}

/// The STW model's proactive interval charges a full throughput drop.
#[test]
fn zs1tp_the_stw_model_charges_proactive_with_a_full_drop() {
    let mut i = base();
    i.cycles = WARM_CYCLES;
    i.used_after_last_cycle = 100 * M;
    i.used = 100 * M + 103 * M;
    i.cycle_avg = 0.01; // HotSpot: 0.49 s; full drop: 0.99 s
    i.since_last_cycle = 0.6;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::Proactive));
    i.stw_model = true;
    assert_eq!(evaluate(&i).fire_now, None, "0.6 s < 0.99 s");
    i.since_last_cycle = 1.0;
    assert_eq!(evaluate(&i).fire_now, Some(DirectorRule::Proactive));
}

#[test]
fn zs1tp_the_stw_model_needs_the_switch_and_no_concurrent_phase() {
    let d = ZDirector::new(1024 * M, true, false);
    assert!(!d.stw_model_active(), "default off");
    d.set_stw_model(true);
    assert!(d.stw_model_active());
    d.set_cycle_shape(true);
    assert!(!d.stw_model_active(), "concurrent marking keeps HotSpot's");
    d.set_cycle_shape(false);
    assert!(d.stw_model_active());
    // The sampled evaluation carries it.
    let t0 = Instant::now();
    d.note_cycle_end(t0, Some(Duration::from_millis(20)), 0);
    d.sample(512 * M, t0 + Duration::from_secs(1));
    let last = d.snapshot().4.unwrap();
    assert_eq!(last.fire_at_rule, Some(DirectorRule::HighUsage));
}

/// The warm-up floor: HotSpot's 10/20/30% points on a small live set,
/// never a zero-progress cycle, and the kill switch is `rearm`.
#[test]
fn zs1tp_the_warmup_floor_is_one_step_of_progress_at_most() {
    let cap = 1024 * M as usize;
    let step = cap / 10;
    let live = cap / 100; // 1%
    let rearm = live + rearm_floor_legacy(cap, live); // ~25.75%
    assert!(rearm > 2 * step, "fixture: the legacy floor is past 20%");
    let f = warmup_floor(live, rearm, step, true);
    assert_eq!(f, live + step / 2);
    assert!(f <= 2 * step, "the second warm-up point (20%) is reachable");
    assert_eq!(warmup_floor(live, rearm, step, false), rearm, "kill");
    // A live set above the next step still needs half a step of
    // progress.
    let big = cap / 2;
    let rearm_big = big + rearm_floor_legacy(cap, big);
    assert_eq!(warmup_floor(big, rearm_big, step, true), big + step / 2);
    // Before the first collection both are zero.
    assert_eq!(warmup_floor(0, 0, step, true), 0);
}
// ---- zr z6/tp ----------------------------------------------------------

#[test]
fn zr6tp_the_near_full_floor_applies_only_above_the_threshold() {
    let cap = 256 * M as usize;
    let thr = cap * 3 / 4;
    // Below the threshold: the legacy quarter of free space.
    let live = cap / 2;
    assert_eq!(rearm_floor(cap, live, thr, 75), (cap - live) / 4);
    // At or above it: 75% of free space.
    let live = cap * 9 / 10;
    let free = cap - live;
    assert_eq!(rearm_floor(cap, live, thr, 75), free * 3 / 4);
    assert_eq!(rearm_floor(cap, thr, thr, 50), (cap - thr) / 2);
    // Kill switch: the legacy floor everywhere.
    assert_eq!(rearm_floor(cap, live, thr, 0), free / 4);
    // Never below the legacy floor, even at a tiny percent.
    assert_eq!(rearm_floor(cap, live, thr, 1), free / 4);
}

#[test]
fn zr6tp_the_near_full_floor_has_a_one_mib_minimum_beyond_free_space() {
    let cap = 256 * M as usize;
    let thr = cap * 3 / 4;
    // 256 KiB free: the legacy floor asks 64 KiB (a collection every
    // 64 KiB, the storm tail); the near-full floor asks 1 MiB, more than is
    // free, so no occupancy clause can fire and the allocation failure
    // collects (the refusal ladder).
    let live = cap - 256 * 1024;
    assert_eq!(rearm_floor_legacy(cap, live), 64 * 1024);
    let floor = rearm_floor(cap, live, thr, 75);
    assert_eq!(floor, REARM_NEAR_FULL_MIN);
    assert!(live + floor > cap);
}

#[test]
fn zr6tp_the_progress_switch_parses_percent_kill_and_words() {
    let d = REARM_NEAR_FULL_DEFAULT_PERCENT;
    assert_eq!(near_full_progress_percent_from(None), d);
    assert_eq!(near_full_progress_percent_from(Some("")), d);
    assert_eq!(near_full_progress_percent_from(Some("0")), 0);
    assert_eq!(near_full_progress_percent_from(Some("off")), 0);
    assert_eq!(near_full_progress_percent_from(Some("false")), 0);
    assert_eq!(near_full_progress_percent_from(Some(" 50 ")), 50);
    assert_eq!(near_full_progress_percent_from(Some("250")), 100);
    assert_eq!(near_full_progress_percent_from(Some("on")), d);
}

/// The pacing model the floor is argued from: a fill that retains ~21% of
/// what it allocates (`ProxyBoxingOomeProbe`'s shape) at 256 MiB, polled
/// every 1400 bytes, warm-up at 10/20/30% and the 75% threshold. The
/// legacy floor reproduces the host's ~150 collections; the near-full
/// floor cuts them by more than a third, and both arms make the same
/// progress before exhaustion: only the count moves, never the OOME point.
#[test]
fn zr6tp_the_fill_model_cuts_collections_and_keeps_the_oome_point() {
    fn fill(pct: usize) -> (usize, u64) {
        let cap = 256 * M as usize;
        let thr = cap * 3 / 4;
        let step = 1400usize;
        let mut live = 0f64;
        let (mut a, mut since, mut gcs, mut rearm) = (0usize, 0usize, 0usize, 0usize);
        let mut steps = 0u64;
        loop {
            let failed = a + step > cap;
            if !failed {
                a += step;
                since += step;
                steps += 1;
            }
            let warm = gcs < 3 && a >= rearm && a >= cap / 10 * (gcs + 1);
            if failed || warm || (a >= rearm && a >= thr) {
                gcs += 1;
                live += since as f64 * 0.21;
                a = live as usize;
                since = 0;
                rearm = a.saturating_add(rearm_floor(cap, a, thr, pct));
                if failed && a + step > cap {
                    return (gcs, steps);
                }
            }
        }
    }
    let (legacy, legacy_steps) = fill(0);
    let (floor, floor_steps) = fill(REARM_NEAR_FULL_DEFAULT_PERCENT);
    assert!(legacy > 120, "legacy model collections {legacy}");
    assert!(
        floor * 3 < legacy * 2,
        "near-full floor {floor} vs legacy {legacy}"
    );
    let d = legacy_steps.abs_diff(floor_steps) as f64 / legacy_steps as f64;
    assert!(d < 0.01, "{legacy_steps} vs {floor_steps}");
}

/// zs s6/no: the zr6tp fill model, extended with the failure tail. Returns
/// (collections, allocation-failure collections, allocation steps before
/// the OOME). `exhausted_floor` = `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR` in
/// bytes.
fn zs6no_fill(pct: usize, exhausted_floor: usize) -> (usize, usize, u64) {
    let cap = 256 * M as usize;
    let thr = cap * 3 / 4;
    let step = 1400usize;
    let mut live = 0f64;
    let (mut a, mut since, mut gcs, mut fails, mut rearm) = (0usize, 0, 0, 0, 0usize);
    let mut steps = 0u64;
    loop {
        let failed = a + step > cap;
        if !failed {
            a += step;
            since += step;
            steps += 1;
        }
        let warm = gcs < 3 && a >= rearm && a >= cap / 10 * (gcs + 1);
        if failed || warm || (a >= rearm && a >= thr) {
            gcs += 1;
            fails += usize::from(failed);
            live += since as f64 * 0.21;
            a = live as usize;
            since = 0;
            let floor = rearm_floor(cap, a, thr, pct);
            rearm = a.saturating_add(floor);
            if failed && (a + step > cap || exhausted_verdict(cap, a, exhausted_floor)) {
                return (gcs, fails, steps);
            }
        }
    }
}

/// The tail the zr6tp page records, in the model: most of the default's
/// collections after the near-full floor took over are allocation
/// failures in the last ~1.33 MiB, and the exhausted verdict at HotSpot's
/// page ends the fill at the first failure collection that leaves less
/// than a page free -- at least 20 collections fewer, for under 1% of the
/// allocation before the OOME (the bytes HotSpot leaves unused).
#[test]
fn zs6no_the_exhausted_verdict_cuts_the_failure_tail_for_a_sliver_of_progress() {
    let (d_gcs, d_fails, d_steps) = zs6no_fill(REARM_NEAR_FULL_DEFAULT_PERCENT, 0);
    let page = EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB * 1024;
    let (v_gcs, v_fails, v_steps) = zs6no_fill(REARM_NEAR_FULL_DEFAULT_PERCENT, page);
    assert!(
        d_fails >= 20,
        "default tail: {d_fails} failure collections of {d_gcs}"
    );
    assert!(
        v_fails <= 2,
        "verdict: {v_fails} failure collections of {v_gcs}"
    );
    assert!(d_gcs >= v_gcs + 20, "default {d_gcs} vs verdict {v_gcs}");
    assert!(v_steps < d_steps);
    let lost = (d_steps - v_steps) as f64 / d_steps as f64;
    assert!(lost < 0.01, "progress lost {lost}");
    // Off is the default: `floor == 0` never decides.
    assert_eq!(zs6no_fill(REARM_NEAR_FULL_DEFAULT_PERCENT, 0).0, d_gcs);
}

#[test]
fn zs6no_the_exhausted_floor_parses_kib_off_and_on_words() {
    assert_eq!(exhausted_free_floor_from(None), 0);
    assert_eq!(exhausted_free_floor_from(Some("")), 0);
    assert_eq!(exhausted_free_floor_from(Some("0")), 0);
    assert_eq!(exhausted_free_floor_from(Some("off")), 0);
    assert_eq!(exhausted_free_floor_from(Some(" 512 ")), 512 * 1024);
    let page = EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB * 1024;
    assert_eq!(exhausted_free_floor_from(Some("on")), page);
    assert_eq!(exhausted_free_floor_from(Some("true")), page);
    assert!(exhausted_verdict(4 * M as usize, 3 * M as usize, page));
    assert!(!exhausted_verdict(4 * M as usize, 2 * M as usize, page));
    assert!(!exhausted_verdict(4 * M as usize, 4 * M as usize, 0));
}

#[test]
fn zr6tp_the_pacing_census_buckets_near_full_collections_only() {
    let c = ZPacingCensus::default();
    c.record(false, PaceKind::Warmup, 30 << 20);
    c.record(true, PaceKind::Threshold, 100 * 1024);
    c.record(true, PaceKind::Threshold, 10 << 20);
    c.record(true, PaceKind::Unasked, 4096);
    c.deferred.fetch_add(2, Ordering::Relaxed);
    assert_eq!(c.near_full_count(), 3);
    assert_eq!(c.near_full_of(PaceKind::Threshold), 2);
    assert_eq!(c.near_full_of(PaceKind::Warmup), 0);
    // zt t3/tp: costs follow the collection `record` saw last.
    c.record(true, PaceKind::Unasked, 2048);
    c.note_cost(50, 5, 20, 25);
    c.record(false, PaceKind::Warmup, 1 << 20);
    c.note_cost(7, 0, 3, 0);
    assert_eq!(c.near_full_us_of(PaceKind::Unasked), 100);
    assert_eq!(c.near_full_us_of(PaceKind::Warmup), 0, "not near full");
    let line = c.line(75);
    for want in [
        "pace_timed=2",
        "pace_us=110",
        "pace_nf_us=100",
        "pace_nf_us_unasked=100",
        "pace_nf_mark_us=50",
        "pace_nf_refs_us=5",
        "pace_nf_sweep_us=20",
        "pace_nf_reloc_us=25",
    ] {
        assert!(line.contains(want), "{want} missing: {line}");
    }
    assert!(
        line.starts_with("[GC] zgc-pacing: pace_collections=6 pace_near_full=4"),
        "{line}"
    );
    for want in [
        "pace_nf_threshold=2",
        "pace_nf_unasked=2",
        "pace_nf_win_lt64k=2",
        "pace_nf_win_lt1m=1",
        "pace_nf_win_lt64m=1",
        "pace_nf_win_min=2048",
        "pace_floor_deferred=2",
        "pace_floor_pct=75",
    ] {
        assert!(line.contains(want), "{want} missing: {line}");
    }
    let keys: Vec<&str> = line
        .split_whitespace()
        .skip(2)
        .map(|kv| kv.split('=').next().unwrap())
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(keys.len(), sorted.len(), "{line}");
}
