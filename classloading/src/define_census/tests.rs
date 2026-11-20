use super::*;

/// `ALL` lists every phase exactly once, so each discriminant indexes its
/// own accumulator and the dump shows every row.
#[test]
fn all_phases_listed_once() {
    let mut seen = [false; PHASES];
    for phase in DefinePhase::ALL {
        assert!(!seen[phase as usize], "{phase:?} listed twice in ALL");
        seen[phase as usize] = true;
    }
    assert!(seen.iter().all(|s| *s));
}

/// zt t6/nd4: the reservation's row exists, is dumped between the
/// supertype pre-pass and the VM's pre-define (where the definers run
/// it), and charges what it is given.
#[test]
fn zt6nd4_mirror_reserve_row_sits_before_the_define_and_charges() {
    let pos = |p: DefinePhase| DefinePhase::ALL.iter().position(|q| *q == p).unwrap();
    assert!(pos(DefinePhase::SupertypePrepass) < pos(DefinePhase::MirrorReserve));
    assert!(pos(DefinePhase::MirrorReserve) < pos(DefinePhase::VmPreDefine));
    assert_eq!(DefinePhase::MirrorReserve.label(), "mirror-reserve");
    let idx = DefinePhase::MirrorReserve as usize;
    let before = PHASE_COUNT[idx].load(Ordering::Relaxed);
    phase_end(DefinePhase::MirrorReserve, Some(std::time::Instant::now()));
    assert_eq!(PHASE_COUNT[idx].load(Ordering::Relaxed), before + 1);
    phase_end(DefinePhase::MirrorReserve, None);
    assert_eq!(
        PHASE_COUNT[idx].load(Ordering::Relaxed),
        before + 1,
        "no clock, no charge"
    );
}

/// zt t30/dc: the late binding's rows sit where their steps run (the wait
/// and the clone around the reservation, the withdraw inside the store
/// add, the upcall / settle / tail rows inside `native-mirror` after its
/// older sub-rows, the bind's lock rows inside the settle), and
/// `phase_charge_ns` charges one occurrence only with the census on.
#[test]
fn zt30dc_bind_rows_sit_where_their_steps_run() {
    let pos = |p: DefinePhase| DefinePhase::ALL.iter().position(|q| *q == p).unwrap();
    assert!(pos(DefinePhase::BindWait) < pos(DefinePhase::MirrorReserve));
    assert!(pos(DefinePhase::MirrorReserve) < pos(DefinePhase::BindRetryClone));
    assert!(pos(DefinePhase::BindRetryClone) < pos(DefinePhase::VmPreDefine));
    assert_eq!(
        pos(DefinePhase::StoreAdd) + 1,
        pos(DefinePhase::BindWithdrawName)
    );
    let mirror = [
        DefinePhase::NativeMirror,
        DefinePhase::MirrorLoaderRecord,
        DefinePhase::MirrorCreate,
        DefinePhase::MirrorAlloc,
        DefinePhase::MirrorFields,
        DefinePhase::MirrorLoaderModule,
        DefinePhase::MirrorPublish,
        DefinePhase::MirrorNativeFields,
        DefinePhase::MirrorAddClass,
        DefinePhase::MirrorBindSettle,
        DefinePhase::BindCmAcquire,
        DefinePhase::BindCmBody,
        DefinePhase::BindRestoreName,
        DefinePhase::BindCmRelease,
        DefinePhase::BindIndex,
        DefinePhase::MirrorAddTail,
        DefinePhase::BindHold,
    ];
    let first = pos(mirror[0]);
    for (k, phase) in mirror.iter().enumerate() {
        assert_eq!(
            pos(*phase),
            first + k,
            "{phase:?} out of the native-mirror block"
        );
    }
    // Nesting by indentation: a sub-row is indented deeper than its parent.
    let depth = |p: DefinePhase| p.label().len() - p.label().trim_start().len();
    assert!(depth(DefinePhase::BindCmBody) > depth(DefinePhase::MirrorBindSettle));
    assert!(depth(DefinePhase::BindRestoreName) > depth(DefinePhase::BindCmBody));
    assert!(depth(DefinePhase::MirrorAddClass) > depth(DefinePhase::NativeMirror));
    assert!(depth(DefinePhase::BindWithdrawName) > depth(DefinePhase::StoreAdd));
    assert_eq!(depth(DefinePhase::BindHold), 0);
    let idx = DefinePhase::BindHold as usize;
    let before = PHASE_COUNT[idx].load(Ordering::Relaxed);
    phase_charge_ns(DefinePhase::BindHold, 40);
    let expect = if census_on() { before + 1 } else { before };
    assert_eq!(PHASE_COUNT[idx].load(Ordering::Relaxed), expect);
}

/// The counter must move on a definition — the whole point is that its
/// predecessor did not, and a replacement that also reads zero would be
/// the same defect with a new name.
#[test]
fn note_increments_the_total() {
    let before = total();
    note("com/example/Probe");
    assert_eq!(
        total(),
        before + 1,
        "define_census::note did not increment; a counter that never moves \
         reports a confident zero, which is exactly what it replaced"
    );
}
