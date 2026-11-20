use super::*;
use std::sync::atomic::Ordering::Relaxed;

fn site() -> JitIndyGenericSite {
    JitIndyGenericSite {
        kind: JIT_INDY_SITE_GENERIC,
        constant_word: std::sync::atomic::AtomicUsize::new(0),
        constant_lambda: false,
        capture_fast: false,
        capture_memo: std::sync::atomic::AtomicU64::new(0),
        class_id: ClassId::new(0x7_2101),
        cp_index: 9,
        target_descriptor: Arc::from("(I)I"),
        arg_types: Arc::from(Vec::<u8>::new().as_slice()),
        frame_class_name: Arc::from(crate::runtime::stackwalker::JIT_INDY_BRIDGE_FRAME_CLASS),
        frame_method_name: Arc::from(JIT_INDY_FRAME_METHOD),
        frame_code: crate::runtime::frame::padded_bytecode(&[]),
        frame_exception_table: Arc::from(
            Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
        ),
        frame_max_stack: 1,
        return_type: b'I',
        singleton_site: (0, 0),
        singleton_slot: std::sync::OnceLock::new(),
        switch_fast: false,
        record_fast: false,
        generic_linked: true,
        linked_direct: true,
        direct: IndyDirectState::default(),
        owner_method: std::sync::OnceLock::new(),
        constant_recompile_requested: std::sync::atomic::AtomicBool::new(false),
    }
}

/// Nothing planned: no arming. Planned another cell: armed with a fresh
/// count. Planned this cell: disarmed. A spent budget never arms.
#[test]
fn a_retarget_arms_only_away_from_the_planned_cell() {
    let s = site();
    s.note_mutable_retarget(0x1000);
    assert!(!s.direct.retarget_armed.load(Relaxed), "no compile planned a cell");
    s.direct.planned_cell.store(0x1000, Relaxed);
    s.direct.retarget_misses.store(77, Relaxed);
    s.note_mutable_retarget(0x2000);
    assert!(s.direct.retarget_armed.load(Relaxed));
    assert_eq!(s.direct.retarget_misses.load(Relaxed), 0);
    s.note_mutable_retarget(0x1000);
    assert!(!s.direct.retarget_armed.load(Relaxed), "back to the planned target");
    s.direct
        .retarget_recompiles
        .store(INDY_MUTABLE_RETARGET_RECOMPILES, Relaxed);
    s.note_mutable_retarget(0x2000);
    assert!(!s.direct.retarget_armed.load(Relaxed), "the budget is spent");
}

/// The threshold-th miss disarms and spends one unit of the budget, once
/// per arming; a site with no owner method asks nothing.
#[test]
fn the_threshold_miss_disarms_once_and_spends_one_recompile() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let s = site();
    s.direct.planned_cell.store(0x1000, Relaxed);
    s.note_mutable_retarget(0x2000);
    for _ in 0..INDY_MUTABLE_RETARGET_MISSES - 1 {
        s.note_retarget_miss(&shared);
    }
    assert!(s.direct.retarget_armed.load(Relaxed), "one short of the threshold");
    assert_eq!(s.direct.retarget_recompiles.load(Relaxed), 0);
    s.note_retarget_miss(&shared);
    assert!(!s.direct.retarget_armed.load(Relaxed));
    assert_eq!(s.direct.retarget_recompiles.load(Relaxed), 1);
    for _ in 0..3 * INDY_MUTABLE_RETARGET_MISSES {
        s.note_retarget_miss(&shared);
    }
    assert_eq!(
        s.direct.retarget_recompiles.load(Relaxed),
        1,
        "a disarmed site's further misses ask nothing"
    );
}

/// A leaked guard cell for the INDY21-1 tests (never dereferenced slots).
fn cell(
    vm: usize,
    owner: usize,
    invoke_kind: u8,
    num_jit_args: usize,
    target_slot: usize,
) -> &'static IndyMutableGuardCell {
    cell_with_row(vm, owner, invoke_kind, num_jit_args, target_slot, 0)
}

/// [`cell`] naming `CallSite.target`'s compact row (mcs21, MCS21-2).
fn cell_with_row(
    vm: usize,
    owner: usize,
    invoke_kind: u8,
    num_jit_args: usize,
    target_slot: usize,
    field_row: u32,
) -> &'static IndyMutableGuardCell {
    let leaf: &'static cratonvm_jit::JitInvokeInfo =
        Box::leak(Box::new(cratonvm_jit::JitInvokeInfo {
            class_name: "p/Host",
            method_name: "addA",
            descriptor: "(I)I",
            num_jit_args,
            return_type: b'I',
            invoke_kind,
            declaring_class_id: 0x7101,
            owner_class_id: 0x7101,
        }));
    Box::leak(Box::new(IndyMutableGuardCell {
        kind: JIT_INDY_SITE_GUARDED_LEAF,
        constant_word: 0,
        owner_site: owner,
        vm,
        call_site_slot: 0x6100,
        target_slot,
        field_index: 3,
        field_row,
        leaf,
        gate: crate::classloading::resolution::RedefineGate::snapshot(Arc::new(
            std::sync::atomic::AtomicU32::new(0),
        )),
    }))
}

/// INDY21-1: a cell site dispatches its leaf when its terms hold, hands
/// the instruction's own site back when they fail, and any other site
/// pointer passes through untouched.
#[test]
fn a_guarded_leaf_cell_dispatches_its_leaf_or_hands_back_its_owner() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let vm: &SharedVm = &shared;
    let vm_ptr = vm as *const SharedVm as usize;
    let owner = site();
    let owner_ptr = &owner as *const JitIndyGenericSite as usize;
    let args: [i64; 1] = [5];
    let null_receiver: [i64; 1] = [0];
    let word = |c: &'static IndyMutableGuardCell| c as *const IndyMutableGuardCell as usize;
    // SAFETY (every call below): live pointers; one argument word.
    unsafe {
        assert!(matches!(jit_indy_guarded_leaf(vm_ptr, 0, args.as_ptr(), 1), (None, 0)));
        assert!(matches!(
            jit_indy_guarded_leaf(vm_ptr, owner_ptr, args.as_ptr(), 1),
            (None, p) if p == owner_ptr
        ));
        let ok = cell(vm.vm_identity, owner_ptr, 3, 1, 0x6200);
        let (leaf, site) = jit_indy_guarded_leaf(vm_ptr, word(ok), args.as_ptr(), 1);
        assert!(leaf.is_some_and(|l| std::ptr::eq(l, ok.leaf)));
        assert_eq!(site, word(ok));
        let wide = cell(vm.vm_identity, owner_ptr, 3, 2, 0x6200);
        assert!(matches!(
            jit_indy_guarded_leaf(vm_ptr, word(wide), args.as_ptr(), 1),
            (None, p) if p == owner_ptr
        ));
        let virt = cell(vm.vm_identity, owner_ptr, 0, 1, 0x6200);
        assert!(matches!(
            jit_indy_guarded_leaf(vm_ptr, word(virt), null_receiver.as_ptr(), 1),
            (None, p) if p == owner_ptr
        ));
        assert!(jit_indy_guarded_leaf(vm_ptr, word(virt), args.as_ptr(), 1)
            .0
            .is_some());
        let other_vm = cell(vm.vm_identity ^ 1, owner_ptr, 3, 1, 0x6200);
        assert!(matches!(
            jit_indy_guarded_leaf(vm_ptr, word(other_vm), args.as_ptr(), 1),
            (None, p) if p == owner_ptr
        ));
    }
}

/// INDY21-1: the second target is the newest other cell of the planned
/// leaf's kind; nothing without a planned cell or with the switch off.
#[test]
fn the_second_target_is_the_newest_other_cell_of_the_planned_kind() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let vm: &SharedVm = &shared;
    let s = site();
    let site_ptr = &s as *const JitIndyGenericSite as usize;
    let word = |c: &'static IndyMutableGuardCell| c as *const IndyMutableGuardCell as usize;
    let a = cell(vm.vm_identity, site_ptr, 3, 1, 0x6201);
    let b = cell(vm.vm_identity, site_ptr, 3, 1, 0x6202);
    let v = cell(vm.vm_identity, site_ptr, 0, 1, 0x6203);
    s.direct.cells.lock().extend([word(a), word(b), word(v)]);
    s.direct.bimorphic_gate.store(2, Relaxed);
    assert_eq!(jit_indy_mutable_second_target(vm, site_ptr), (0, 0), "nothing planned");
    s.direct.planned_cell.store(word(b), Relaxed);
    if jit_indy_mutable_guard_enabled() {
        assert_eq!(
            jit_indy_mutable_second_target(vm, site_ptr),
            (0x6201, word(a)),
            "the virtual cell is skipped, the planned one too"
        );
    }
    s.direct.planned_cell.store(word(a), Relaxed);
    if jit_indy_mutable_guard_enabled() {
        assert_eq!(jit_indy_mutable_second_target(vm, site_ptr), (0x6202, word(b)));
    }
    s.direct.bimorphic_gate.store(1, Relaxed);
    assert_eq!(jit_indy_mutable_second_target(vm, site_ptr), (0, 0), "switch off");
}

/// INDY21 closure: the threshold doubles per recompile asked and stops at
/// its cap.
#[test]
fn indy21_the_retarget_threshold_doubles_per_recompile_and_stops_at_the_cap() {
    assert_eq!(retarget_miss_threshold(0), INDY_MUTABLE_RETARGET_MISSES);
    assert_eq!(retarget_miss_threshold(1), 2 * INDY_MUTABLE_RETARGET_MISSES);
    assert_eq!(retarget_miss_threshold(3), 8 * INDY_MUTABLE_RETARGET_MISSES);
    let cap = INDY_MUTABLE_RETARGET_MISSES << INDY_MUTABLE_RETARGET_BACKOFF_MAX_SHIFT;
    assert_eq!(retarget_miss_threshold(INDY_MUTABLE_RETARGET_BACKOFF_MAX_SHIFT), cap);
    assert_eq!(retarget_miss_threshold(INDY_MUTABLE_RETARGET_RECOMPILES), cap);
    assert_eq!(retarget_miss_threshold(u32::MAX), cap);
}

/// INDY21 closure: a site that flips back (the ping-pong the r22 A/B
/// priced at 1.16-1.18x with the switch alone) asks its second recompile
/// only after twice the bridge calls of its first.
#[test]
fn indy21_a_ping_pong_site_asks_its_second_recompile_at_twice_the_misses() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let s = site();
    s.direct.planned_cell.store(0x1000, Relaxed);
    s.note_mutable_retarget(0x2000);
    for _ in 0..INDY_MUTABLE_RETARGET_MISSES {
        s.note_retarget_miss(&shared);
    }
    assert_eq!(s.direct.retarget_recompiles.load(Relaxed), 1, "the plain threshold");
    // The recompile planned the target the site held (0x2000); it flips back.
    s.direct.planned_cell.store(0x2000, Relaxed);
    s.note_mutable_retarget(0x1000);
    assert!(s.direct.retarget_armed.load(Relaxed));
    for _ in 0..INDY_MUTABLE_RETARGET_MISSES {
        s.note_retarget_miss(&shared);
    }
    assert!(s.direct.retarget_armed.load(Relaxed), "still armed at the plain threshold");
    assert_eq!(s.direct.retarget_recompiles.load(Relaxed), 1);
    for _ in 0..INDY_MUTABLE_RETARGET_MISSES {
        s.note_retarget_miss(&shared);
    }
    assert!(!s.direct.retarget_armed.load(Relaxed));
    assert_eq!(s.direct.retarget_recompiles.load(Relaxed), 2, "twice the misses");
}

/// INDY21 closure: a target with no guard cell disarms the site, and the
/// planned cell moves only through the note the door makes after its
/// admission (a null or foreign pointer is ignored).
#[test]
fn indy21_a_refused_target_disarms_and_a_plan_is_recorded_only_through_the_note() {
    let s = site();
    s.direct.planned_cell.store(0x1000, Relaxed);
    s.note_mutable_retarget(0x2000);
    assert!(s.direct.retarget_armed.load(Relaxed));
    s.disarm_retarget();
    assert!(!s.direct.retarget_armed.load(Relaxed));
    let ptr = &s as *const JitIndyGenericSite as usize;
    jit_indy_mutable_note_planned(ptr, 0x3000);
    assert_eq!(s.direct.planned_cell.load(Relaxed), 0x3000);
    jit_indy_mutable_note_planned(0, 0x4000);
    let foreign = cell(0, ptr, 3, 1, 0x6200);
    jit_indy_mutable_note_planned(foreign as *const IndyMutableGuardCell as usize, 0x5000);
    assert_eq!(s.direct.planned_cell.load(Relaxed), 0x3000, "unchanged");
}



/// mcs21 review: a site's cell budget stops at the cap and the count stays
/// there (a bare `fetch_add` kept counting past it and wrapped after 2^32
/// asks, minting more never-released cells).
#[test]
fn mcs21_the_guard_cell_budget_stops_at_the_cap_and_stays_there() {
    let minted = std::sync::atomic::AtomicU32::new(0);
    for _ in 0..INDY_DIRECT_MAX_FILLS {
        assert!(claim_guard_cell_budget(&minted));
    }
    for _ in 0..4 {
        assert!(!claim_guard_cell_budget(&minted), "the cap is spent");
    }
    assert_eq!(minted.load(Relaxed), INDY_DIRECT_MAX_FILLS, "the count stays at the cap");
    let wrapped = std::sync::atomic::AtomicU32::new(u32::MAX);
    assert!(
        !claim_guard_cell_budget(&wrapped),
        "an extreme count neither overflows nor re-opens the budget"
    );
    assert_eq!(wrapped.load(Relaxed), u32::MAX);
}

/// mcs21 (MCS21-2): no row without a compact layout to read it from (a
/// class id no layout was registered for, or a slot far out of range).
#[test]
fn mcs21_a_class_without_a_compact_layout_names_no_guard_row() {
    // An id far above any class a test process registers.
    let unregistered = ClassId::new(0x7F00_0021);
    assert_eq!(guard_target_field_row(unregistered, 1), 0);
    assert_eq!(guard_target_field_row(unregistered, u32::MAX), 0);
}

/// mcs21 (MCS21-2): the compile door hands the cell's row over as the
/// sixth element of the guard answer, with the cell word fifth.
#[test]
fn mcs21_the_guard_answer_carries_the_cells_row() {
    if !jit_indy_mutable_guard_enabled() {
        return;
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let vm: &SharedVm = &shared;
    let s = site();
    let site_ptr = &s as *const JitIndyGenericSite as usize;
    let word = |c: &'static IndyMutableGuardCell| c as *const IndyMutableGuardCell as usize;
    let named = cell_with_row(vm.vm_identity, site_ptr, 3, 1, 0x6200, 17);
    s.direct.mutable_cell.store(word(named), Relaxed);
    let answer = jit_indy_mutable_guard_target(vm, site_ptr).expect("a cell of this VM");
    assert_eq!((answer.4, answer.5), (word(named), 17));
    let bare = cell_with_row(vm.vm_identity, site_ptr, 3, 1, 0x6200, 0);
    s.direct.mutable_cell.store(word(bare), Relaxed);
    let answer = jit_indy_mutable_guard_target(vm, site_ptr).expect("a cell of this VM");
    assert_eq!((answer.4, answer.5), (word(bare), 0));
}
