use super::*;
use crate::classloading::ClassId;
use crate::config::VmConfig;
// FIX: `Heap` import removed — the locals/stack root tests now allocate
// from `shared.mem.heap` (the heap actually scanned) instead of an orphan
// `Heap::new()`, so the standalone `Heap` type is no longer referenced.
use crate::runtime::frame::Frame;
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

fn test_shared_vm() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(VmConfig::default()))
}

/// **THE THIRD INPUT IS THE ONE THAT MATTERS.**
///
/// A `UserDefined` class whose mirror `mirror_pin` does not name has no
/// replacement root, so deferring it roots it NOWHERE. That is the shape
/// measured on `module/spring-boot-flyway …
/// ResourceProviderCustomizerBeanRegistrationAotProcessorTests`
/// (`CRATONVM_DBG=mirrorpin`): 5 `add_mirror_pin` rows against 752
/// distinct mirrors reconciled `is_marked=false`, and
/// `NoSuchMethodError: 'boolean
/// java.lang.String.isAssignableFrom(java.lang.Class)'` out the far end.
#[test]
fn a_user_defined_mirror_with_no_mirror_pin_row_is_not_deferred() {
    assert!(
        !mirror_deferral_is_covered(true, true, false),
        "a mirror the propagation does not name must stay in the root set; deferring it frees a `java.lang.Class` that is still in use"
    );
}

/// The deferral still happens when the propagation really can reach it —
/// this is what keeps `class_loader_unload_regression` green.
#[test]
fn a_user_defined_mirror_the_propagation_names_is_still_deferred() {
    assert!(mirror_deferral_is_covered(true, true, true));
}

/// Both other inputs stay load-bearing: a built-in-loader class is rooted
/// outright, and so is any mirror on a cycle whose marker will not follow
/// the side tables.
#[test]
fn a_builtin_mirror_and_an_unfollowed_cycle_are_both_rooted() {
    assert!(!mirror_deferral_is_covered(false, true, true));
    assert!(!mirror_deferral_is_covered(true, false, true));
}

/// gc-common w18-d: a non-strong hidden class's mirror is deferred only
/// when the class is registered at THIS mirror and its instance edge
/// (`loader_pin`) names the same mirror, on a cycle whose marker follows
/// that edge. Any one missing roots it.
#[test]
fn a_non_strong_hidden_mirror_is_deferred_only_with_its_instance_edge() {
    const M: usize = 0x18D_7000;
    assert!(non_strong_hidden_deferral_is_covered(
        true,
        Some(M),
        Some(M),
        M
    ));
    assert!(!non_strong_hidden_deferral_is_covered(
        false,
        Some(M),
        Some(M),
        M
    ));
    assert!(!non_strong_hidden_deferral_is_covered(
        true,
        None,
        Some(M),
        M
    ));
    assert!(
        !non_strong_hidden_deferral_is_covered(true, Some(M), Some(0x18D_7100), M),
        "an instance edge naming a loader (or a stale address) does not cover the mirror"
    );
    assert!(!non_strong_hidden_deferral_is_covered(
        true,
        Some(0x18D_7100),
        Some(M),
        M
    ));
    assert!(!non_strong_hidden_deferral_is_covered(
        true,
        Some(M),
        None,
        M
    ));
}

/// gc-common w18-d
/// (`common-w8e-non-strong-hidden-classes-unload-only-with-their-loader`):
/// the root scan of a VM with a registered non-strong hidden class.
/// Its mirror is NOT an unconditional root on a cycle that follows the
/// side edges (a strong built-in class's mirror still is), it IS a root
/// while an activation of the class is on the stack, and the class's
/// instance edge names the mirror, so a live instance keeps the class.
#[test]
fn a_non_strong_hidden_mirror_lives_by_its_instances_and_activations() {
    let shared = test_shared_vm();
    let vm = shared.vm_identity;
    // Class ids no class manager of this VM holds and no other test uses.
    let hidden = ClassId::new(0x7ff1_8d01);
    let strong = ClassId::new(0x7ff1_8d02);
    let hidden_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let strong_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    {
        let mut mirrors = shared.classes.class_mirrors.write();
        mirrors.insert(hidden, hidden_mirror);
        mirrors.insert(strong, strong_mirror);
    }
    cratonvm_native_builtins::classloader::register_non_strong_hidden_mirror(
        &shared.loader_identity,
        hidden.as_u32(),
        addr_of(hidden_mirror),
        None,
    );
    assert_eq!(
        vm_loader_pin_addr(&shared, hidden.as_u32()),
        Some(addr_of(hidden_mirror)),
        "a live instance of the class must mark its mirror"
    );

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let conditional = conditional_loader_metadata(&shared);
    let deferrable = shared
        .mem
        .heap
        .mirror_pin_deferrable(addr_of(hidden_mirror));
    let roots = collect_roots(&shared, &thread);
    assert!(
        roots.contains(&strong_mirror),
        "a built-in class's mirror stays rooted"
    );
    if conditional && deferrable {
        assert!(
            !roots.contains(&hidden_mirror),
            "a non-strong hidden class's mirror must be able to die while its loader lives"
        );
    } else {
        assert!(
            roots.contains(&hidden_mirror),
            "no deferral licensed: rooted as before"
        );
    }

    thread.frames.push(Frame::new(
        hidden,
        "w18d/Hidden/0x1".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        vec![],
        vec![],
        4,
        4,
        &[],
    ));
    assert!(
        collect_roots(&shared, &thread).contains(&hidden_mirror),
        "an activation of the class keeps its mirror, whatever the cycle"
    );

    cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm);
}

/// gc-common w19-a
/// (`common-w18d-peer-interpreter-activations-do-not-keep-their-class`):
/// `frame_class_owners`, the list both peer deposits publish, names each
/// frame's defining user loader ONCE however many frames share it, and a
/// non-strong hidden class's mirror. A bootstrap frame adds nothing, and
/// another VM's row at the same class id is not this VM's owner.
#[test]
fn frame_class_owners_names_each_activation_owner_once() {
    use cratonvm_native_builtins::classloader as cl;
    let shared = test_shared_vm();
    let other = test_shared_vm();
    let vm = shared.vm_identity;
    assert_ne!(vm, other.vm_identity);
    // Class ids no class manager holds and no other test uses.
    let user = ClassId::new(0x7ff1_9a01);
    let hidden = ClassId::new(0x7ff1_9a02);
    let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let other_loader = other.mem.heap.alloc_object(ClassId::new(0), 0);
    cl::register_defining_loader(&shared.loader_identity, user.as_u32(), loader);
    cl::register_defining_loader(&other.loader_identity, hidden.as_u32(), other_loader);
    cl::register_non_strong_hidden_mirror(
        &shared.loader_identity,
        hidden.as_u32(),
        addr_of(mirror),
        None,
    );
    let frame = |cid: ClassId| {
        Frame::new(
            cid,
            "w19a/Owner".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            4,
            &[],
        )
    };

    let mut thread = JvmThread::new(ThreadId(0), "w19a-owners");
    assert!(
        frame_class_owners(&shared.loader_identity, &thread).is_empty(),
        "no frame, no owner"
    );
    thread.frames.push(frame(ClassId::new(0)));
    thread.frames.push(frame(user));
    thread.frames.push(frame(hidden));
    thread.frames.push(frame(user));
    let owners = frame_class_owners(&shared.loader_identity, &thread);
    // gce e1/j: the in-native deposit's per-class path gives the same list.
    assert_eq!(frame_class_owners_per_class(&shared.loader_identity, &thread), owners);
    assert_eq!(
        crate::jit::conservative_roots::with_frozen_band_memo(None, || {
            frame_class_owners(&shared.loader_identity, &thread)
        }),
        owners
    );

    cl::forget_vm_loader_singletons(vm);
    cl::forget_vm_loader_singletons(other.vm_identity);
    cratonvm_types::loader_pin::forget_vm_loader_pins(vm);
    cratonvm_types::loader_pin::forget_vm_loader_pins(other.vm_identity);

    assert_eq!(
        owners.iter().filter(|o| **o == loader).count(),
        1,
        "two activations of one user-loader class publish its loader once: {owners:?}"
    );
    assert!(
        owners.contains(&mirror),
        "a non-strong hidden activation keeps its mirror"
    );
    assert!(
        !owners.contains(&other_loader),
        "another VM's defining row at the same class id is not this VM's owner"
    );
    assert_eq!(owners.len(), 2, "{owners:?}");
}

/// gci i17/dk (i16/dp D2): `frame_class_owners` (both arms) gathers on the
/// stack and returns exactly the list the old two-`Vec` body built, in the
/// same order: inline for a handful of owners, spilled (same contents) past
/// 8 owners and 32 distinct classes.
#[test]
fn gci_i17dk_frame_class_owners_stay_on_the_stack_with_the_same_list() {
    use cratonvm_native_builtins::classloader as cl;
    // The pre-i17 body, verbatim but for the two `Vec`s it kept.
    fn old_owners(vm: &cl::LoaderIdentityTables, thread: &JvmThread) -> Vec<ObjectRef> {
        let hidden = cl::non_strong_hidden_mirrors(vm);
        let mut owners: Vec<ObjectRef> = Vec::new();
        let mut asked: Vec<u32> = Vec::new();
        for frame in thread.frames.iter() {
            let class_id = frame.class_id.as_u32();
            if asked.contains(&class_id) {
                continue;
            }
            asked.push(class_id);
            if let Some(&mirror) = hidden.get(&class_id) {
                // SAFETY: a mirror the test registered and keeps alive.
                owners.push(unsafe { ObjectRef::from_raw(mirror as *mut u8) });
            }
            if let Some(loader) = cl::defining_loader_for(vm, class_id) {
                if !owners.contains(&loader) {
                    owners.push(loader);
                }
            }
        }
        owners
    }
    let shared = test_shared_vm();
    let vm = &shared.loader_identity;
    let frame = |cid: ClassId| {
        Frame::new(
            cid,
            "i17dk/Owner".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            4,
            &[],
        )
    };
    // Class ids no class manager holds and no other test uses.
    let base = 0x7ff1_9b00u32;
    let hidden = ClassId::new(base + 0x80);
    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    cl::register_non_strong_hidden_mirror(vm, hidden.as_u32(), addr_of(mirror), None);
    let mut loaders = Vec::new();
    for i in 0..12u32 {
        let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        cl::register_defining_loader(vm, base + i, loader);
        loaders.push(loader);
    }

    // A handful: two user classes (one loader each), the hidden class and
    // a bootstrap frame, with repeats.
    let mut thread = JvmThread::new(ThreadId(0), "i17dk-owners");
    for cid in [0, base, base + 0x80, base + 1, base, base + 1, 0] {
        thread.frames.push(frame(ClassId::new(cid)));
    }
    let small = frame_class_owners(vm, &thread);
    let small_old = old_owners(vm, &thread);
    let small_per_class = frame_class_owners_per_class(vm, &thread);

    // Wide: 12 user-loader classes, the hidden class and 40 ids with no
    // owner (53 distinct classes, 13 owners).
    let mut wide_thread = JvmThread::new(ThreadId(0), "i17dk-owners-wide");
    for i in 0..12u32 {
        wide_thread.frames.push(frame(ClassId::new(base + i)));
        wide_thread.frames.push(frame(ClassId::new(0x7ff1_9c00 + i)));
    }
    wide_thread.frames.push(frame(hidden));
    for i in 12..40u32 {
        wide_thread.frames.push(frame(ClassId::new(0x7ff1_9c00 + i)));
    }
    let wide = frame_class_owners(vm, &wide_thread);
    let wide_old = old_owners(vm, &wide_thread);
    let wide_per_class = frame_class_owners_per_class(vm, &wide_thread);

    cl::forget_vm_loader_singletons(shared.vm_identity);
    cratonvm_types::loader_pin::forget_vm_loader_pins(shared.vm_identity);

    assert_eq!(small.as_slice(), small_old.as_slice(), "same owners, same order");
    assert_eq!(small_per_class.as_slice(), small_old.as_slice());
    assert_eq!(small.as_slice(), &[loaders[0], mirror, loaders[1]][..]);
    assert!(!small.spilled(), "a handful of owners stays on the stack");
    assert!(!small_per_class.spilled());

    assert_eq!(wide.as_slice(), wide_old.as_slice(), "spilled: same owners, same order");
    assert_eq!(wide_per_class.as_slice(), wide_old.as_slice());
    assert_eq!(wide.len(), 13, "{wide:?}");
    assert!(wide.spilled(), "13 owners exceed the inline 8");
}

/// gci i18/dk (i17/dk K1): every deposit takes the per-class arm, and it
/// returns exactly what the removed default arm (a `HashMap` snapshot of
/// all this VM's non-strong hidden rows) returned, in the same order: with
/// another VM's hidden row and loader at this VM's class ids, with this VM
/// holding hidden rows and no defining loader, and with this VM holding
/// nothing while the process-wide hidden flag is up. Inside and outside
/// the in-native deposit's arming the answer is the same list.
#[test]
fn gci_i18dk_every_deposit_takes_the_per_class_arm() {
    use cratonvm_native_builtins::classloader as cl;
    // The removed default arm, verbatim but for its `SmallVec`s.
    fn old_default_arm(vm: &cl::LoaderIdentityTables, thread: &JvmThread) -> Vec<ObjectRef> {
        let hidden = cl::non_strong_hidden_mirrors(vm);
        if hidden.is_empty() && !cl::any_defining_loader_registered(vm) {
            return Vec::new();
        }
        let mut owners: Vec<ObjectRef> = Vec::new();
        let mut asked: Vec<u32> = Vec::new();
        for frame in thread.frames.iter() {
            let class_id = frame.class_id.as_u32();
            if asked.contains(&class_id) {
                continue;
            }
            asked.push(class_id);
            if let Some(&mirror) = hidden.get(&class_id) {
                // SAFETY: a mirror the test registered and keeps alive.
                owners.push(unsafe { ObjectRef::from_raw(mirror as *mut u8) });
            }
            if let Some(loader) = cl::defining_loader_for(vm, class_id) {
                if !owners.contains(&loader) {
                    owners.push(loader);
                }
            }
        }
        owners
    }
    let frame = |cid: u32| {
        Frame::new(
            ClassId::new(cid),
            "i18dk/Owner".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            4,
            &[],
        )
    };
    let both_arms = |vm: &cl::LoaderIdentityTables, thread: &JvmThread| {
        let plain = frame_class_owners(vm, thread);
        let armed = crate::jit::conservative_roots::with_frozen_band_memo(None, || {
            frame_class_owners(vm, thread)
        });
        (plain.to_vec(), armed.to_vec())
    };

    // Class ids no class manager holds and no other test uses.
    let base = 0x7ff1_9d00u32;
    let shared = test_shared_vm();
    let other = test_shared_vm();
    let hidden_only = test_shared_vm();
    let empty = test_shared_vm();
    let vm = &shared.loader_identity;
    // This VM: two loaders, two hidden classes (one also loader-defined).
    let l0 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let l1 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let m0 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let m1 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    cl::register_defining_loader(vm, base, l0);
    cl::register_defining_loader(vm, base + 1, l1);
    cl::register_defining_loader(vm, base + 2, l0);
    cl::register_non_strong_hidden_mirror(vm, base + 2, addr_of(m0), None);
    cl::register_non_strong_hidden_mirror(vm, base + 3, addr_of(m1), None);
    // Another VM's rows at the same ids: never this VM's owners.
    let ol = other.mem.heap.alloc_object(ClassId::new(0), 0);
    let om = other.mem.heap.alloc_object(ClassId::new(0), 0);
    cl::register_defining_loader(&other.loader_identity, base + 4, ol);
    cl::register_non_strong_hidden_mirror(&other.loader_identity, base + 4, addr_of(om), None);
    cl::register_non_strong_hidden_mirror(&other.loader_identity, base, addr_of(om), None);
    // A VM with hidden rows and no defining loader of its own.
    let hm = hidden_only.mem.heap.alloc_object(ClassId::new(0), 0);
    cl::register_non_strong_hidden_mirror(&hidden_only.loader_identity, base + 5, addr_of(hm), None);

    let mut thread = JvmThread::new(ThreadId(0), "i18dk-owners");
    for cid in [0, base + 3, base, base + 4, base + 2, base + 1, base, base + 5, base + 3] {
        thread.frames.push(frame(cid));
    }
    let (mine, mine_armed) = both_arms(vm, &thread);
    let mine_old = old_default_arm(vm, &thread);
    let (hid, hid_armed) = both_arms(&hidden_only.loader_identity, &thread);
    let hid_old = old_default_arm(&hidden_only.loader_identity, &thread);
    let (none, none_armed) = both_arms(&empty.loader_identity, &thread);
    let none_old = old_default_arm(&empty.loader_identity, &thread);

    for v in [shared.vm_identity, other.vm_identity, hidden_only.vm_identity, empty.vm_identity] {
        cl::forget_vm_loader_singletons(v);
        cratonvm_types::loader_pin::forget_vm_loader_pins(v);
    }

    assert_eq!(mine, mine_old, "same owners, same order");
    assert_eq!(mine_armed, mine_old);
    assert_eq!(mine, vec![m1, l0, m0, l1], "frame order, each loader once");
    assert!(!mine.contains(&ol) && !mine.contains(&om), "another VM's rows stay out");
    assert_eq!(hid, hid_old);
    assert_eq!(hid_armed, hid_old);
    assert_eq!(hid, vec![hm], "hidden rows without a defining loader");
    assert_eq!(none, none_old);
    assert_eq!(none_armed, none_old);
    assert!(none.is_empty(), "a VM with no rows has no owners: {none:?}");
}

/// gci i17/dk (i16/dp D4): the initiator's pin list (`JitPinAddrs`) gets
/// the same reject pins appended, in the same order, as the `Vec` it
/// replaces did through `extend_published`; an empty reject list appends
/// nothing; the list stays on the stack below 32 addresses.
#[test]
fn gci_i17dk_initiator_pin_list_appends_the_same_reject_pins() {
    use crate::jit::conservative_roots::g1_band_reject_pins as brp;
    let shared = test_shared_vm();
    let pins = [
        brp::RejectPin {
            addr: 0x17d0_0040,
            humongous: true,
            region: 7,
        },
        brp::RejectPin {
            addr: 0x17d0_0080,
            humongous: false,
            region: 9,
        },
        brp::RejectPin {
            addr: 0x17d0_00c0,
            humongous: true,
            region: 7,
        },
    ];
    let scanned = [0x1000usize, 0x2000, 0x1000];

    let mut old: Vec<usize> = scanned.to_vec();
    brp::extend_published(&pins, &mut old);
    let mut new: JitPinAddrs = scanned.iter().copied().collect();
    publish_g1_band_reject_pins(&shared.mem.heap, &pins, &mut new, false);
    assert_eq!(new.as_slice(), old.as_slice());
    assert!(!new.spilled());

    let mut none: JitPinAddrs = scanned.iter().copied().collect();
    publish_g1_band_reject_pins(&shared.mem.heap, &[], &mut none, false);
    assert_eq!(none.as_slice(), &scanned[..], "no reject pin, nothing appended");
}

/// **THE OWNER'S OWN SCAN ROOTS ITS NATIVE ALLOCATION POOL.**
///
/// Both peer snapshots published `native_alloc_pool`; `collect_roots` did
/// not, so a collection initiated by the pool's owner (the common case — the
/// pool is refilled exactly when young space is exhausted) left up to 2047
/// pre-allocated old-generation objects unrooted, and an old-generation
/// sweep freed blocks the pool would later hand out as fresh objects.
#[test]
fn the_native_allocation_pool_is_an_initiator_root() {
    let shared = test_shared_vm();
    let pooled = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mut thread = JvmThread::new(ThreadId(0), "test");

    assert!(
        !collect_roots(&shared, &thread).contains(&pooled),
        "an object with no reference must not already be a root, or the \
         assertion below proves nothing"
    );

    thread.native_alloc_pool.push(pooled);
    assert!(
        collect_roots(&shared, &thread).contains(&pooled),
        "a pooled object is live until the pool hands it out; the owner's \
         scan must root it exactly as both peer snapshots do"
    );
}

/// **THE TWO PARKED THROWABLES ARE PUBLISHED BY THE SHARED OFF-FRAME HELPER.**
///
/// `push_off_frame_thread_roots` is the one list all three per-thread root
/// paths call. `jit_pending_exception` and `uncaught_exception_pending` were
/// pushed by the initiator's §10 only, so a peer-initiated collection (the
/// shutdown-hook case: the launcher sleeps in a blocking region while hook
/// threads allocate) never marked them.
#[test]
fn the_parked_throwables_are_off_frame_roots_on_every_path() {
    // SAFETY: never dereferenced — the helper only copies addresses.
    let jit_exc = unsafe { ObjectRef::from_raw(0x7000usize as *mut u8) };
    let uncaught = unsafe { ObjectRef::from_raw(0x7100usize as *mut u8) };
    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.jit_pending_exception = Some(jit_exc);
    thread.uncaught_exception_pending = Some(uncaught);

    let mut roots = Vec::new();
    push_off_frame_thread_roots(&thread, &mut roots);
    assert!(
        roots.contains(&jit_exc),
        "jit_pending_exception must be published"
    );
    assert!(
        roots.contains(&uncaught),
        "uncaught_exception_pending must be published"
    );
}

/// **ANOTHER VM'S LOADER PIN NEVER UNROOTS THIS VM'S VALUE**
/// (`common-w4b-loader-pin-collision-unroots-another-vms-statics-FIXED-20261010.md`,
/// gc-common w4-b).
///
/// `loader_pin` is keyed by the bare class id, so VM A's user-loader class
/// and VM B's class at the same id share one row. A deferral in B keyed by
/// A's loader address is followed by no marker of B's: the value is simply
/// unrooted. The deferral must use a pin only when THIS VM's own
/// defining-loader table agrees.
#[test]
fn a_colliding_class_ids_loader_pin_is_used_only_by_its_own_vm() {
    let vm_a = test_shared_vm();
    let vm_b = test_shared_vm();
    assert_ne!(vm_a.vm_identity, vm_b.vm_identity);
    // A class id no other test registers.
    let cid: u32 = 0x7ff0_4c01;
    let loader_a = vm_a.mem.heap.alloc_object(ClassId::new(0), 0);
    cratonvm_native_builtins::classloader::register_defining_loader(
        &vm_a.loader_identity,
        cid,
        loader_a,
    );
    assert_eq!(
        cratonvm_types::loader_pin::loader_pin_addr(cid),
        Some(loader_a.as_ptr() as usize),
        "the process-wide row names A's loader (non-vacuity)"
    );

    assert_eq!(
        vm_loader_pin_addr(&vm_a, cid),
        Some(loader_a.as_ptr() as usize),
        "A's own pin must still license A's deferral, or class unloading regresses"
    );
    assert_eq!(
        vm_loader_pin_addr(&vm_b, cid),
        None,
        "B has no user-loader class at this id: its value must be ROOTED, not \
         pinned to a loader in A's heap that B's marker never visits"
    );

    // gc-common w5-b: when B DOES have a user-loader class at the same id,
    // each VM defers to its OWN loader whichever wrote last -- the w4-b
    // cross-check rooted the earlier writer's values instead.
    let loader_b = vm_b.mem.heap.alloc_object(ClassId::new(0), 0);
    cratonvm_native_builtins::classloader::register_defining_loader(
        &vm_b.loader_identity,
        cid,
        loader_b,
    );
    assert_eq!(
        vm_loader_pin_addr(&vm_a, cid),
        Some(loader_a.as_ptr() as usize),
        "A's pin survives B's registration at the same id"
    );
    assert_eq!(
        vm_loader_pin_addr(&vm_b, cid),
        Some(loader_b.as_ptr() as usize)
    );

    cratonvm_types::loader_pin::forget_vm_loader_pins(vm_a.vm_identity);
    cratonvm_types::loader_pin::forget_vm_loader_pins(vm_b.vm_identity);
}

/// gen r5w4/conc8
/// (`gengc-r5w3-unload7-orphaned-class-statics-deferred-to-a-dead-loader`):
/// a class with a user `loader_pin` row that the unload transaction would
/// never unload (held by the store, `ClassManager::unloads_on_hint` false)
/// must not have its loader-conditional roots deferred: they would die with
/// the loader while the class, and its slots, stay. The scan's screen is
/// what refuses it; this fixture's id is NOT in the store, so (orchestrator,
/// r5w4 merge) the class manager itself does not refuse it -- an absent id
/// has nothing the transaction would leave behind -- and the refusal is
/// driven through the screen.
#[test]
fn a_class_the_unload_never_removes_does_not_defer_its_roots() {
    let shared = test_shared_vm();
    let vm = shared.vm_identity;
    // A class id no class manager holds and no other test uses.
    let cid: u32 = 0x7ff2_c801;
    let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    cratonvm_native_builtins::classloader::register_defining_loader(
        &shared.loader_identity,
        cid,
        loader,
    );
    let pin = vm_loader_pin_addr(&shared, cid);
    let unscreened = vm_deferral_owner(&shared, cid);
    let refused_set = classes_whose_roots_never_defer(&shared);
    let screened_refusing = {
        let _screen = DeferralScreen::enter(vm, [cid].into_iter().collect());
        vm_deferral_owner(&shared, cid)
    };
    let screened_allowing = {
        let _screen = DeferralScreen::enter(vm, rustc_hash::FxHashSet::default());
        vm_deferral_owner(&shared, cid)
    };
    let foreign_screen = {
        let _screen = DeferralScreen::enter(vm ^ 1, rustc_hash::FxHashSet::default());
        vm_deferral_owner(&shared, cid)
    };
    let restored = DEFERRAL_REFUSED.with(|c| c.borrow().is_none());

    cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm);
    cratonvm_types::loader_pin::forget_vm_loader_pins(vm);

    assert_eq!(pin, Some(addr_of(loader)), "the pin exists (non-vacuity)");
    assert_eq!(
        unscreened,
        Some(addr_of(loader)),
        "an id the store does not hold is not refused by the class manager"
    );
    assert!(!refused_set.contains(&cid), "nor named by the scan's set");
    assert_eq!(screened_refusing, None);
    assert_eq!(
        screened_allowing,
        Some(addr_of(loader)),
        "a screened scan defers exactly what its set allows"
    );
    assert_eq!(
        foreign_screen,
        Some(addr_of(loader)),
        "another VM's screen does not answer for this VM (the class manager does)"
    );
    assert!(restored, "every screen restores the previous (empty) one");
}

/// **A JIT MEMO CACHE IS ROOTED WHILE PRESENT AND EMPTIED AFTER A PAUSE**
/// (`docs/feature-designs/jit-ir-optimizations.md`, gc-common
/// w4-b).
///
/// Three halves, and each is load-bearing:
///
/// * a present entry IS a root -- dropping that would free a `node` the
///   next probe returns (the TOMCAT-JNDIREALM-JIT.3 use-after-free);
/// * at the generation it was validated at, neither the publish-side nor
///   the probe-side gate touches it -- the memo still hits within one;
/// * one pause later the publish-side gate EMPTIES it -- that is the
///   retention fix: a thread that stopped probing no longer keeps a
///   dropped `HashMap` alive until 32 newer entries evict it.
#[test]
fn a_jit_memo_cache_is_rooted_while_present_and_emptied_after_a_pause() {
    use crate::threading::jvm_thread::{JitHashMapStringNodeCacheEntry, StringCaseCacheEntry};
    // SAFETY: never dereferenced — the scan only copies addresses.
    let fake = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let shared = test_shared_vm();
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Start where a thread that just probed is: validated at "now".
    validate_jit_memo_caches(&shared, &mut thread);
    assert_eq!(thread.jit_memo_epoch, jit_memo_epoch_now(&shared));
    thread
        .jit_hashmap_string_node_cache
        .push(JitHashMapStringNodeCacheEntry {
            map: fake(0x7200),
            node: fake(0x7300),
            key_object: Some(fake(0x7400)),
            key: String::new(),
            mod_count_slot: 0,
            mod_count: 0,
            chm_generation: None,
            chm_segment_id: None,
        });
    thread.string_case_cache.push(StringCaseCacheEntry {
        source: fake(0x7500),
        locale: None,
        upper: false,
        first: fake(0x7600),
        second: fake(0x7700),
        next: false,
    });

    let mut roots = Vec::new();
    push_off_frame_thread_roots(&thread, &mut roots);
    let roots: Vec<usize> = roots.iter().map(|o| o.as_ptr() as usize).collect();
    for a in [0x7200usize, 0x7300, 0x7400, 0x7500, 0x7600, 0x7700] {
        assert!(
            roots.contains(&a),
            "a present memo entry (0x{a:x}) must be a root, or the next hit \
             returns a reclaimed node"
        );
    }

    // Same generation: both gates are no-ops.
    drop_stale_jit_memo_caches(&shared, &mut thread);
    validate_jit_memo_caches(&shared, &mut thread);
    assert_eq!(
        (
            thread.jit_hashmap_string_node_cache.len(),
            thread.string_case_cache.len()
        ),
        (1, 1),
        "a cache validated at the current generation must still hit"
    );

    // A pause completes: the next publish empties both and re-arms.
    shared
        .mem
        .gc_barrier
        .gc_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    drop_stale_jit_memo_caches(&shared, &mut thread);
    assert!(
        thread.jit_hashmap_string_node_cache.is_empty() && thread.string_case_cache.is_empty(),
        "a memo from before the last pause must not be published as a root again"
    );
    assert_eq!(thread.jit_memo_epoch, jit_memo_epoch_now(&shared));

    // The probe-side gate does the same for a thread that probes first.
    thread.string_case_cache.push(StringCaseCacheEntry {
        source: fake(0x7800),
        locale: None,
        upper: true,
        first: fake(0x7900),
        second: fake(0x7A00),
        next: false,
    });
    shared
        .mem
        .gc_barrier
        .gc_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    validate_jit_memo_caches(&shared, &mut thread);
    assert!(
        thread.string_case_cache.is_empty(),
        "a probe after a pause must not be served from before it"
    );
}

/// **A SYNCHRONIZED FRAME'S MONITOR OBJECT IS A ROOT OF ITS OWNER'S SCAN.**
///
/// `update_all_roots` rewrote `monitor_on_exit` and the blocked deposit
/// published it, but the initiator's scan relied on the receiver still
/// sitting in local 0 — which a static synchronized method's lock object
/// never does.
#[test]
fn a_frames_monitor_on_exit_is_an_initiator_root() {
    let shared = test_shared_vm();
    let lock = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mut frame = Frame::new(
        ClassId::new(0),
        "TestClass".to_string(),
        "test".to_string(),
        "()V".to_string(),
        None,
        vec![],
        vec![],
        10,
        5,
        &[],
    );
    frame.monitor_on_exit = Some(lock);
    thread.frames.push(frame);

    assert!(
        collect_roots(&shared, &thread).contains(&lock),
        "the object a frame will monitorexit on pop must survive the collection"
    );
}

/// **A FRAME'S BLOCK MONITORS ARE ROOTS OF ITS OWNER'S SCAN** (JVMS
/// §2.11.10 structured locking, interpreter round i1 wave 23, lane L7).
///
/// Hand-written bytecode (`dup; monitorenter`) keeps the locked object in
/// no local; the frame's `held_monitors` record is then its only copy, and
/// `monitorexit` finds the entry by address.
#[test]
fn a_frames_held_monitors_are_initiator_roots() {
    let shared = test_shared_vm();
    let lock = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mut frame = Frame::new(
        ClassId::new(0),
        "TestClass".to_string(),
        "test".to_string(),
        "()V".to_string(),
        None,
        vec![],
        vec![],
        10,
        5,
        &[],
    );
    frame.held_monitors.push(lock);
    thread.frames.push(frame);

    assert!(
        collect_roots(&shared, &thread).contains(&lock),
        "an object a frame's monitorenter locked must survive the collection"
    );
}

fn addr_of(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// **A QUEUED `Reference` NOTHING HOLDS IS GARBAGE** (gc-common w2-b).
///
/// Step 22 used to root every pending queued `Reference` in the process,
/// which kept `java.io.ClassCache.CacheRef` — a queued `SoftReference`
/// whose strong `type` field names the `Class` it caches for — and with it
/// every user class loader that ever reached `ObjectStreamClass.lookup`,
/// alive until memory pressure. `class_loader_unload_regression` failed on
/// all three collectors because of it. See
/// [`push_pending_references_held_by_frames`].
#[test]
fn a_queued_reference_nothing_holds_is_not_a_root() {
    let shared = test_shared_vm();
    let reference = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    shared.mem.ref_processor.lock().discover_reference(
        cratonvm_gc::ReferenceType::Soft,
        addr_of(reference),
        addr_of(referent),
        Some(addr_of(queue)),
    );
    // Non-vacuity: the processor does report it as pending — it is the
    // ROOT SCAN that must decline it.
    assert!(
        shared
            .mem
            .ref_processor
            .lock()
            .pending_reference_object_addresses()
            .contains(&addr_of(reference)),
        "the fixture must register a pending queued reference"
    );
    let thread = JvmThread::new(ThreadId(0), "test");
    assert!(
        !collect_roots(&shared, &thread).contains(&reference),
        "an unreachable registered Reference must die (java.lang.ref: it is \
         then never enqueued), not be kept alive with every field it holds"
    );
}

/// **A USER-LOADER PROXY'S CACHED `Method` IS PINNED TO ITS LOADER, NOT
/// ROOTED** (gc-common w2-b) — whenever the cycle's marker follows
/// `metadata_pin`. Rooting it outright kept the proxy's loader alive
/// forever (the `Method`'s `clazz` names the proxied interface), and the
/// row was only ever dropped by the unload that rooting prevented.
#[test]
fn a_user_loader_proxy_method_is_pinned_to_its_loader_not_rooted() {
    let shared = test_shared_vm();
    let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let method = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let bootstrap_method = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    // A class id no other test registers a loader pin for.
    let proxy_cid = ClassId::new(0x7ff0_2b01);
    let bootstrap_proxy_cid = ClassId::new(0x7ff0_2b02);
    // Through the authoritative side table (which also writes the
    // `loader_pin` row): a deferral uses a pin only when THIS VM's own
    // defining-loader row agrees (`vm_loader_pin_addr`, gc-common w4-b).
    cratonvm_native_builtins::classloader::register_defining_loader(
        &shared.loader_identity,
        proxy_cid.as_u32(),
        loader,
    );
    {
        let mut cache = shared.classes.proxy_method_cache.write();
        cache.insert((proxy_cid, "go".into(), "(I)I".into()), method);
        cache.insert(
            (bootstrap_proxy_cid, "go".into(), "(I)I".into()),
            bootstrap_method,
        );
    }
    let thread = JvmThread::new(ThreadId(0), "test");
    let conditional = conditional_loader_metadata(&shared);
    let roots = collect_roots(&shared, &thread);
    assert!(
        roots.contains(&bootstrap_method),
        "a proxy with no defining-loader pin has nothing to defer to and stays rooted"
    );
    if conditional {
        assert!(
            !roots.contains(&method),
            "a user-loader proxy's Method must not be an unconditional root"
        );
        assert!(
            cratonvm_types::metadata_pin::roots_for_loader(addr_of(loader))
                .is_some_and(|pinned| pinned.contains(&addr_of(method))),
            "...it must be pinned to the proxy class's defining loader instead"
        );
    } else {
        assert!(
            roots.contains(&method),
            "no deferral licensed: rooted as before"
        );
    }
    cratonvm_types::loader_pin::forget_vm_loader_pins(shared.vm_identity);
    cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
}

/// **A DEFERRED CLEANER ACTION IS A ROOT** (handoff-d, applied in w2-b).
///
/// The queue holds raw addresses across collections; without this a
/// sweeping collection frees the cleanable and the later drain writes into
/// and invokes reclaimed memory.
#[test]
fn a_deferred_cleaner_action_is_an_initiator_root() {
    let shared = test_shared_vm();
    let cleanable = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    shared.mem.cleaner_thread.submit_action(addr_of(cleanable));
    let thread = JvmThread::new(ThreadId(0), "test");
    assert!(
        collect_roots(&shared, &thread).contains(&cleanable),
        "a cleanable whose action is queued but not yet run must survive the collection"
    );
    // Once drained (the action ran) it is no longer this scan's business.
    let _ = shared.mem.cleaner_thread.drain_actions();
    assert!(!collect_roots(&shared, &thread).contains(&cleanable));
}

/// **…BUT ONE A FRAME STILL HOLDS IN A LIVENESS-DEAD LOCAL IS KEPT.**
///
/// `r = new WeakReference<>(x, q); System.gc(); q.remove()` with `r` never
/// read again: the per-bci liveness filter drops `r`, where HotSpot's
/// interpreter keeps it. The rescue is limited to QUEUED references — a
/// queue-less one has nothing to deliver, so a dead local holding it stays
/// dead.
#[test]
fn a_queued_reference_in_a_liveness_dead_local_is_still_a_root() {
    let shared = test_shared_vm();
    let alloc = || shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let (queued, queued_referent, queue) = (alloc(), alloc(), alloc());
    let (unqueued, unqueued_referent) = (alloc(), alloc());
    {
        let mut rp = shared.mem.ref_processor.lock();
        rp.discover_reference(
            cratonvm_gc::ReferenceType::Weak,
            addr_of(queued),
            addr_of(queued_referent),
            Some(addr_of(queue)),
        );
        rp.discover_reference(
            cratonvm_gc::ReferenceType::Weak,
            addr_of(unqueued),
            addr_of(unqueued_referent),
            None,
        );
    }
    // `return` at pc 0: no later bytecode reads slot 1 or 2, so both are
    // dead to the liveness filter (slot 0 is always kept).
    let frame = Frame::new(
        ClassId::new(0),
        "TestClass".to_string(),
        "w2bQueuedRefInDeadLocal".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        4,
        4,
        &[
            Value::Int(0),
            Value::Object(Some(queued)),
            Value::Object(Some(unqueued)),
        ],
    );
    // Non-vacuity: the liveness-filtered scan really drops both slots, so
    // whatever roots `queued` below is step 22 and not step 1.
    let mut filtered = Vec::new();
    frame.scan_local_objects(&mut filtered, &shared.mem.heap);
    assert!(
        !filtered.contains(&queued) && !filtered.contains(&unqueued),
        "fixture: slots 1 and 2 must be liveness-dead at a `return`"
    );
    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.frames.push(frame);

    let roots = collect_roots(&shared, &thread);
    assert!(
        roots.contains(&queued),
        "a pending queued Reference held by a live frame must survive to be enqueued"
    );
    // With the conservative probe engaged step 1 roots every local anyway;
    // only the liveness-filtered configuration can show the decline.
    if !conservative_locals_enabled() {
        assert!(
            !roots.contains(&unqueued),
            "a queue-less Reference in a dead local has nothing to deliver and must not be rescued"
        );
    }
}

/// **THE CONCURRENT OLD-GEN MARK VETOES EVERY SIDE-TABLE DEFERRAL.**
///
/// `ConcurrentMarker` follows no side table, so a root set its remark
/// consumes must root statics, class locks, condy values, `ClassValue`
/// results and user-loader mirrors outright — whatever the JIT or an
/// explicit `System.gc()` would otherwise license.
///
/// gen r5w3/unload7: a FOURTH input, `class_unload_marking`, now licenses
/// the concurrent cycle's own initial-mark and remark scans when its
/// marker follows the side tables (`CRATONVM_GEN_CONC_CLASS_UNLOAD`). The
/// assertions below that pass `false` for it are the pre-unload7 truth
/// table, unchanged.
#[test]
fn the_concurrent_old_gen_mark_vetoes_conditional_metadata() {
    // Outside a concurrent mark: the two licensing terms behave as before.
    assert!(!generational_metadata_is_conditional(
        false, false, false, false
    ));
    assert!(generational_metadata_is_conditional(
        false, true, false, false
    ));
    assert!(generational_metadata_is_conditional(
        false, false, true, false
    ));
    // Inside one: never conditional, whatever licenses it — unless the
    // scan is the cycle's own class-unload scan.
    for jit in [false, true] {
        for major in [false, true] {
            assert!(
                !generational_metadata_is_conditional(true, jit, major, false),
                "jit_active={jit} major_gc_requested={major}: a remark root \
                 set must not defer to side tables its marker never reads"
            );
            for open in [false, true] {
                assert!(
                    generational_metadata_is_conditional(open, jit, major, true),
                    "open={open} jit_active={jit} major_gc_requested={major}: a \
                     class-unload scan's marker follows the side tables"
                );
            }
        }
    }
}

#[test]
fn metadata_weak_mode_forces_in_place_only_when_term_four_is_disarmed() {
    // The one case the fix exists for: weak mode licensed by the JIT-active
    // term alone, a moving collector, and no conservative-root divert.
    assert!(metadata_weak_mode_needs_in_place_promise(
        true, false, true, false
    ));
    // Every other combination is the pre-fix behaviour: no extra divert.
    for weak in [false, true] {
        for explicit in [false, true] {
            for moving in [false, true] {
                for term4 in [false, true] {
                    let expected = weak && !explicit && moving && !term4;
                    assert_eq!(
                        metadata_weak_mode_needs_in_place_promise(
                            weak, explicit, moving, term4
                        ),
                        expected,
                        "weak={weak} explicit={explicit} moving={moving} term4={term4}"
                    );
                }
            }
        }
    }
}

/// gen r5w6/conc10 — `CRATONVM_GEN_YOUNG_MIRROR_DEFER` widens step 6 only
/// on a Generational scan with the flag on, young deferral not opted out,
/// outside the concurrent cycle's class-unload scans unless that scan is
/// itself consuming the side-table snapshot; `CRATONVM_DBG_FORCE_MOVING`
/// only vetoes the young-collection promise.
#[test]
fn r5w6_young_mirror_defer_rule_truth_table() {
    assert!(young_mirror_defer_rule(true, true, true, false, false));
    for generational in [false, true] {
        for flag in [false, true] {
            for allowed in [false, true] {
                for unload_scan in [false, true] {
                    for force_moving in [false, true] {
                        let expected = generational
                            && flag
                            && allowed
                            && (unload_scan || !force_moving);
                        assert_eq!(
                            young_mirror_defer_rule(
                                generational,
                                flag,
                                allowed,
                                unload_scan,
                                force_moving
                            ),
                            expected,
                            "gen={generational} flag={flag} allowed={allowed} \
                             unload_scan={unload_scan} force_moving={force_moving}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn concurrent_unload_mirror_deferral_does_not_set_a_young_collection_promise() {
    assert!(!young_mirror_needs_young_promise(true));
    assert!(young_mirror_needs_young_promise(false));
}

/// gcj j3/yg3 — `CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE`: a licensed
/// scan stops widening step 6 to young mirrors only when the switch is on
/// AND the scan publishes the precise-table counts; with either off the
/// conc10 widening (and 14w''s forced in-place promise) is kept.
#[test]
fn gcj_j3yg3_young_mirrors_are_rooted_only_when_the_scan_publishes_precise_counts() {
    assert!(!young_mirror_widens(true, true), "the default fix");
    assert!(young_mirror_widens(false, true), "the =0 kill keeps conc10");
    assert!(young_mirror_widens(true, false), "PRP off keeps conc10");
    assert!(young_mirror_widens(false, false));
    // The widening is still licensed first: an unlicensed scan widens
    // nothing whatever this rule says.
    for root_when_promotable in [false, true] {
        for prp in [false, true] {
            let licensed = young_mirror_defer_rule(false, true, true, false, false);
            assert!(!(licensed && young_mirror_widens(root_when_promotable, prp)));
        }
    }
}

/// gcj j3/yg3 -- `CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS`: the
/// promise is the forced in-place sweep unless the switch asks the moving
/// branch to follow the rows; step 14w' sets the collector's flag on that
/// arm and the root gatherer clears it at the top of every scan.
#[test]
fn gcj_j3yg3_the_young_mirror_promise_rule_and_its_wiring() {
    assert_eq!(young_mirror_promise(false), YoungMirrorPromise::ForceInPlace);
    assert_eq!(young_mirror_promise(true), YoungMirrorPromise::MovingFollowsRows);
    // gcj yg3-residuals R2: default ON, `=0` kills.
    let on = |v: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(
            &[(YOUNG_MIRROR_DEFER_MOVING_FOLLOWS_FLAG, v)],
            young_mirror_defer_moving_follows_enabled,
        )
    };
    assert!(on(None), "default ON");
    assert!(!on(Some("0")), "=0 kills");
    assert!(on(Some("1")));
    assert_eq!(
        YOUNG_MIRROR_DEFER_MOVING_FOLLOWS_FLAG,
        "CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS"
    );
    let src = include_str!("../roots.rs");
    let clear = concat!("cratonvm_gc::gen_heap::clear_young_side_table", "_rows_follow();");
    let set = concat!("cratonvm_gc::gen_heap::set_young_side_table", "_rows_follow();");
    let step6 = concat!("let young_mirror_defer = young_mirror_defer", "_licensed(shared)");
    assert_eq!(src.matches(clear).count(), 1, "cleared once per scan");
    assert_eq!(src.matches(set).count(), 1, "set by step 14w' only");
    let (c, d, s6) = (
        src.find(clear).expect("clear"),
        src.find(set).expect("set"),
        src.find(step6).expect("step 6"),
    );
    assert!(c < s6 && s6 < d, "cleared before step 6, set after it");
    // The flag itself (gc crate), through an alias so these calls are not
    // the needles above.
    use cratonvm_gc::gen_heap as gh;
    gh::set_young_side_table_rows_follow();
    assert!(gh::young_side_table_rows_follow());
    gh::clear_young_side_table_rows_follow();
    assert!(!gh::young_side_table_rows_follow());
}

/// gcj j3/yg3 — step 6 asks the rule with THIS scan's PRP licence (the
/// `precise_root_promote` local of `collect_roots`), so a scan that
/// publishes no table keeps the deferral. Needles are split with
/// `concat!` so they cannot match this test's own text.
#[test]
fn gcj_j3yg3_step6_asks_the_widening_rule_with_the_scans_prp_licence() {
    let src = include_str!("../roots.rs");
    let call = concat!(
        "let young_mirror_defer = young_mirror_defer_licensed(shared)\n",
        "                && young_mirror",
        "_widens(\n",
        "                    young_mirror_root_when_promotable_enabled(),\n",
        "                    precise_root",
        "_promote,\n"
    );
    assert_eq!(src.matches(call).count(), 1, "step 6 asks the rule once");
    let licence = concat!(
        "let precise_root_promote = precise_root",
        "_promote_licensed(shared);"
    );
    let at_licence = src.find(licence).expect("the PRP licence local");
    let at_call = src.find(call).expect("the step 6 call");
    assert!(at_licence < at_call, "the licence is read before step 6");
    assert_eq!(
        YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE_FLAG,
        "CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE"
    );
}

#[test]
fn roots_from_frame_locals() {
    let shared = test_shared_vm();
    // FIX: allocate from `shared.mem.heap` (the heap the scanner validates
    // against via `is_object_address`), not an orphan `Heap::new()`.
    // The frame scanner drops any slot whose address is not resident in
    // the heap being scanned — a foreign-heap object can never be a root.
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let frame = Frame::new(
        ClassId::new(0),
        "TestClass".to_string(),
        "test".to_string(),
        "()V".to_string(),
        None,
        vec![],
        vec![],
        10,
        5,
        &[Value::Object(Some(obj)), Value::Int(42)],
    );
    thread.frames.push(frame);

    let roots = collect_roots(&shared, &thread);
    assert!(roots.contains(&obj));
}

/// **THE A5 PASS RECOVERS WHAT THE TAG-FILTERED SCAN IS DESIGNED TO DROP.**
///
/// A JIT callee's object return value can reach an interpreter local under
/// a non-object tag. `scan_local_objects` then correctly omits it — a
/// `long`-tagged slot is a primitive by JVM spec, and rooting it on the
/// MOVING path would let the collector rewrite a number. The non-moving
/// sweep has no such hazard and must not free the object, which is what
/// `scan_locals_conservative` exists for.
///
/// Both halves are asserted. "The conservative pass finds it" alone would
/// pass just as well if the ordinary scan already did, and then the pass
/// this test is about could be deleted without failing anything.
#[test]
fn the_a5_pass_recovers_a_lost_tag_local_the_tag_filtered_scan_drops() {
    let shared = test_shared_vm();
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as usize;

    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.frames.push(Frame::new(
        ClassId::new(0),
        "TestClass".to_string(),
        "test".to_string(),
        "()V".to_string(),
        None,
        vec![],
        vec![],
        10,
        5,
        // The lost tag: the address is in the slot, but as a `long`.
        &[Value::Long(addr as i64), Value::Int(42)],
    ));

    assert!(
        !collect_roots(&shared, &thread).contains(&obj),
        "a long-tagged slot must NOT be a root on the ordinary path — if it \
         is, the moving collector would rewrite a primitive, and step 14a5 \
         is not what keeps this object alive"
    );

    let mut roots = Vec::new();
    conservative_frame_pass(&shared, &thread, &mut roots);
    assert!(
        roots.contains(&obj),
        "the A5 pass must recover the reference: on that cycle the sweep \
         frees on GC_FLAG_MARKED, so a slot no scan reports is a slot whose \
         object is zeroed and returned to the free list while still in use"
    );
}

#[test]
fn the_a5_pass_excludes_dead_locals_without_losing_live_untagged_refs() {
    let shared = test_shared_vm();
    let dead = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let live = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Slot 1 has had its final read, while slot 2 is loaded after the
    // collection point. Keep both tagged and lost-tag cases honest.
    // 0: astore_1; 1: aload_2; 2: pop; 3: goto 1
    let mut frame = Frame::new(
        ClassId::new(0),
        "TestClass".into(),
        "test".into(),
        "()V".into(),
        None,
        vec![0x4c, 0x2c, 0x57, 0xa7, 0xff, 0xfe],
        vec![],
        2,
        3,
        &[
            Value::Int(0),
            Value::Object(Some(dead)),
            Value::Long(live.as_ptr() as i64),
        ],
    );
    frame.pc = 1;
    frame.last_instr_pc = 1;
    thread.frames.push(frame);
    let mut roots = Vec::new();
    // gci i10/np: "excludes dead locals" is the liveness-only arm
    // (`CRATONVM_GC_INTERP_TYPE_MAP_LOCALS=0`). On the default arm slot 1
    // is still a REFERENCE at the loop head (the store and the back edge
    // both carry it), so HotSpot's interpreter type map -- and this pass,
    // through the same frame scan -- keeps it.
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_GC_INTERP_TYPE_MAP_LOCALS", Some("0"))],
        || conservative_frame_pass(&shared, &thread, &mut roots),
    );
    assert!(!roots.contains(&dead), "dead tagged local prevented finalization");
    assert!(roots.contains(&live), "live lost-tag reference was not pinned");
    roots.clear();
    cratonvm_types::flags::with_thread_overrides(&[], || {
        conservative_frame_pass(&shared, &thread, &mut roots)
    });
    assert!(roots.contains(&dead), "default arm: a dead but reference-typed local is kept");
    assert!(roots.contains(&live));
    roots.clear();
    thread.frames[0].set_local_compact_unchecked(
        1,
        cratonvm_types::CompactValue::long(dead.as_ptr() as i64),
    );
    roots.clear();
    conservative_frame_pass(&shared, &thread, &mut roots);
    assert!(!roots.contains(&dead), "dead lost-tag local was revived");
    assert!(roots.contains(&live));
    // The union must also protect a reference consumed by the instruction
    // that initiated a call/safepoint, even after pc advanced past it.
    thread.frames[0].pc = 2;
    thread.frames[0].last_instr_pc = 1;
    roots.clear();
    conservative_frame_pass(&shared, &thread, &mut roots);
    assert!(roots.contains(&live));
}

/// **AND IT RUNS ON EXACTLY THE CYCLES THAT NEED IT.**
///
/// The pass is only sound where nothing is relocated, and only useful where
/// step 1's probe was off. Both conditions are in one predicate so the
/// truth table can be pinned here; the flag it reads is set from inside
/// `collect_roots`, so an end-to-end test cannot reach this state.
#[test]
fn the_a5_pass_engages_only_on_the_unregistered_jit_frame_path() {
    // The flag is a `thread_local!` `Cell`, so this cannot perturb a test
    // running beside it. Restore it either way.
    let restore = cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack();

    cratonvm_gc::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    assert!(
        !a5_frame_pass_engages(false),
        "with no unregistered JIT frame the young cycle may MOVE, and a \
         pointer-shaped long rooted by this pass would be relocated and \
         corrupted"
    );

    cratonvm_gc::gc_quiescence::set_unregistered_jit_frame_on_stack();
    assert_eq!(
        a5_frame_pass_engages(false),
        conservative_locals_compiled_in(),
        "with the A5 flag set the sweep is non-moving and the pass must run \
         — unless the probe is compiled out of this run entirely"
    );
    assert!(
        !a5_frame_pass_engages(true),
        "step 1 already probed these frames; running again only doubles the \
         root vector"
    );

    if !restore {
        cratonvm_gc::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    }
}

#[test]
fn roots_from_frame_stack() {
    let shared = test_shared_vm();
    // FIX: allocate from `shared.mem.heap` so the operand-stack scanner's
    // `is_heap_addr` validation recognizes the objects as live heap
    // residents. Objects from a disconnected `Heap::new()` are correctly
    // rejected by the scanner and would never appear as roots.
    let obj1 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let obj2 = shared.mem.heap.alloc_object(ClassId::new(0), 0);

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mut frame = Frame::new(
        ClassId::new(0),
        "TestClass".to_string(),
        "test".to_string(),
        "()V".to_string(),
        None,
        vec![],
        vec![],
        10,
        2,
        &[],
    );
    frame.stack.push(Value::Object(Some(obj1))).unwrap();
    frame.stack.push(Value::Int(5)).unwrap();
    frame.stack.push(Value::Object(Some(obj2))).unwrap();
    thread.frames.push(frame);

    let roots = collect_roots(&shared, &thread);
    assert!(roots.contains(&obj1));
    assert!(roots.contains(&obj2));
}

#[test]
fn roots_from_statics() {
    let shared = test_shared_vm();
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

    {
        let mut statics = shared.classes.statics.write();
        statics.insert(
            ClassId::new(1),
            crate::vm::realms::class_realm::StaticsBlock::from_values(vec![
                Value::Object(Some(obj)),
                Value::Int(0),
            ]),
        );
    }

    let thread = JvmThread::new(ThreadId(0), "test");
    let roots = collect_roots(&shared, &thread);
    assert!(roots.contains(&obj));
}

/// The scan records the ADDRESS of every static slot holding an object, and
/// `update_all_roots` takes that list exactly once.
///
/// Both halves matter and they fail differently. If the list omits a slot,
/// the post-collection fix-up leaves a live static pointing at a vacated
/// address -- a use-after-free that surfaces arbitrarily far away. If the
/// list can be taken twice, a collection with no scan of its own applies a
/// list from an earlier one, which misses every slot that became
/// object-valued in between -- the same failure by a different route. The
/// take is what rules the second one out: an `update_all_roots` with no
/// preceding scan gets `None` and does the full walk.
#[test]
fn the_scan_records_static_ref_slots_and_the_take_is_once() {
    if !static_root_slots_enabled() {
        return; // kill switch set in this environment; nothing to assert
    }
    let shared = test_shared_vm();
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

    // Two object slots and two primitives, so a list that simply recorded
    // every slot would be distinguishable from one that recorded the
    // reference-holding ones.
    let (slot0, slot2) = {
        let mut statics = shared.classes.statics.write();
        let block = crate::vm::realms::class_realm::StaticsBlock::from_values(vec![
            Value::Object(Some(obj)),
            Value::Int(7),
            Value::Object(Some(obj)),
            Value::Long(9),
        ]);
        let base = block.base_ptr();
        statics.insert(ClassId::new(1), block);
        // SAFETY: `base` is the start of a leaked `[Value; 4]`.
        (base as usize, unsafe { base.add(2) } as usize)
    };

    let thread = JvmThread::new(ThreadId(0), "test");
    let _ = collect_roots(&shared, &thread);

    // gc-common w7-a: another VM's fix-up on this thread must not take
    // (and apply) this VM's list, and must not consume it either.
    assert!(
        take_static_ref_slots(shared.vm_identity.wrapping_add(1_000_003)).is_none(),
        "a fix-up of another VM must fall back to its own full walk"
    );
    let slots = take_static_ref_slots(shared.vm_identity)
        .expect("the scan must record the two object slots");
    assert!(
        slots.contains(&slot0) && slots.contains(&slot2),
        "both object-valued slots must be recorded; got {slots:?} want {slot0:#x} and {slot2:#x}"
    );

    assert!(
        take_static_ref_slots(shared.vm_identity).is_none(),
        "the take must be once -- a second consumer would be applying a list from a scan that is not its own"
    );
}

#[test]
fn roots_from_printed() {
    let shared = test_shared_vm();
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

    let mut thread = JvmThread::new(ThreadId(0), "test");
    thread.printed.push(Value::Object(Some(obj)));
    thread.printed.push(Value::Int(100));

    let roots = collect_roots(&shared, &thread);
    assert!(roots.contains(&obj));
}

#[test]
fn roots_empty_state() {
    let shared = test_shared_vm();
    let thread = JvmThread::new(ThreadId(0), "test");
    let roots = collect_roots(&shared, &thread);
    assert!(
        roots.iter().all(|root| shared
            .mem
            .heap
            .is_object_address(root.as_ptr() as usize)
            .is_none()),
        "empty SharedVm/thread state must not contribute roots from this heap: {roots:?}",
    );
}

/// NEW-1.5 end-to-end: an object whose only live reference lives on the
/// native stack (in a fake "JIT spill slot") is discovered by the
/// conservative scanner and reported as a root, provided a JIT entry
/// guard is active. Without the guard, the object is *not* discovered
/// (the chain is empty) — confirming that we don't accidentally scan
/// the entire native stack on every root collection.
#[test]
fn conservative_jit_root_scan_finds_spilled_object() {
    let shared = test_shared_vm();
    let obj = shared
        .mem
        .heap
        .alloc_object(crate::classloading::ClassId::new(0), 0);
    let thread = JvmThread::new(ThreadId(0), "test");

    // Place the object's address into a stack-allocated slot. The
    // conservative scanner walks `[scanner_sp .. entry_sp)` for each
    // active JIT entry, treating each qword as a possible heap pointer.
    // We push a JIT entry guard FIRST (so entry_sp is captured *above*
    // this point in the stack), then allocate the spill slot below it,
    // then run the scan from inside this same scope so the scanner's
    // own SP is even further down the stack.
    let _g = crate::jit::conservative_roots::JitEntryGuard::enter();

    // Keep the spill slot live via std::hint::black_box so the
    // optimizer can't elide it and the address remains observable.
    let mut spill_slot: usize = obj.as_ptr() as usize;
    std::hint::black_box(&mut spill_slot);

    let roots = collect_roots(&shared, &thread);
    // The object should appear in the root set via the conservative
    // JIT scan path. We can't assert exact length because the scan
    // may also pick up unrelated stack values that coincidentally
    // look like object headers — but those are filtered by
    // is_object_address so the count is bounded.
    assert!(
        roots.contains(&obj),
        "conservative JIT root scan must report the spilled object as a root \
         (heap addr = {:#x}, roots = {:?})",
        obj.as_ptr() as usize,
        roots
            .iter()
            .map(|r| r.as_ptr() as usize)
            .collect::<Vec<_>>()
    );
    // Hint to the compiler that spill_slot is still live at this point
    // — otherwise it might be reused by an earlier register before the
    // scan runs and we'd get a false negative.
    std::hint::black_box(&spill_slot);
}

/// Negative companion to the above: with NO active JIT entry guard,
/// the conservative scanner sees an empty chain and must not report
/// the object as a root (it has no other reference path).
#[test]
fn conservative_jit_root_scan_skips_inactive_threads() {
    let shared = test_shared_vm();
    let obj = shared
        .mem
        .heap
        .alloc_object(crate::classloading::ClassId::new(0), 0);
    let thread = JvmThread::new(ThreadId(0), "test");

    let mut spill_slot: usize = obj.as_ptr() as usize;
    std::hint::black_box(&mut spill_slot);

    let roots = collect_roots(&shared, &thread);
    assert!(
        !roots.contains(&obj),
        "with no JIT entry guard, the conservative scanner must not \
         scan the calling thread's native stack"
    );
}

/// NEW-1.5 GC quiescence: while a JIT entry guard is held, the
/// process-wide quiescence flag is set, telling the GC to defer
/// compaction.
#[test]
fn jit_entry_sets_gc_quiescence_flag() {
    let depth_before = cratonvm_gc::gc_quiescence::depth();
    {
        let _g = crate::jit::conservative_roots::JitEntryGuard::enter();
        assert_eq!(cratonvm_gc::gc_quiescence::depth(), depth_before + 1);
        assert!(cratonvm_gc::gc_quiescence::is_active());
    }
    assert_eq!(cratonvm_gc::gc_quiescence::depth(), depth_before);
}
/// **The coverage proof must be computed for EVERY collector, not only the
/// one that consumes the suppression.**
///
/// This is a source witness, and it is a source witness on purpose. The
/// defect it guards is not a wrong value — it is a call that never happens:
/// `refresh_moving_young_coverage_for_collection()` sat behind
/// `heap.is_generational()` in a short-circuiting `&&` chain, so on a G1 or
/// ZGC cycle nothing ran and `moving_young_coverage_incomplete()` kept the
/// `false` that `begin_moving_young_coverage_cycle` had reset it to. A
/// collector-side gate reading that verdict is reading a proof nobody ran,
/// which is exactly how `zgc::relocate_stw`'s first per-cycle refusal was
/// unsound. No runtime assertion in this crate can see a call that did not
/// happen; the ORDER of the terms is the invariant, so the order is what is
/// asserted.
#[test]
fn the_coverage_proof_runs_for_every_collector() {
    let src = include_str!("../roots.rs");

    // Establish the corpus before concluding anything from it: a file that
    // stopped containing the decision would make this pass for the wrong
    // reason.
    assert!(
        src.contains("let coverage_proven = moving_young"),
        "roots.rs no longer computes `coverage_proven`, so this scan is \
         reading the wrong text and its verdict means nothing"
    );

    let proof = src
        .find("let coverage_proven = moving_young")
        .expect("anchor checked above");
    let suppression = src
        .find("let moving_young_precise_only =")
        .expect("roots.rs no longer computes `moving_young_precise_only`");
    assert!(
        proof < suppression,
        "the proof must be computed BEFORE, and independently of, the \
         suppression decision"
    );

    // The proof expression must not mention the collector at all. If a
    // collector test creeps back into it, `&&` short-circuits and the
    // refresh stops running for whatever the test excludes.
    let proof_expr = &src[proof..suppression];
    assert!(
        proof_expr.contains("refresh_moving_young_coverage_for_collection("),
        "`coverage_proven` no longer calls the refresh, so nothing computes \
         the verdict any collector-side gate reads"
    );
    // AN ARGUMENT IS NOT A TERM, and the difference is the whole of what
    // this guard protects.
    //
    // The refresh takes the collector's `honours_conservative_pins()` as a
    // parameter, so the proof expression does mention the collector -- and
    // must, because whether a PIN discharges a peer's coverage obligation
    // is a question only the collector can answer. What the loop below
    // forbids is a collector test as a `&&` TERM, which is a different
    // thing: `&&` short-circuits, so a term would stop the refresh RUNNING
    // for whatever it excludes, and that is precisely the defect this
    // scan was written for. A parameter cannot do that -- the call is
    // made either way.
    for forbidden in ["is_generational", "is_g1", "g1_precise_only_roots"] {
        assert!(
            !proof_expr.contains(forbidden),
            "`coverage_proven` mentions `{forbidden}`. In a short-circuiting \
             `&&` chain that stops the refresh from running for the excluded \
             collectors, and their verdict silently becomes a vacuous \
             `false` meaning `complete` -- the defect this split exists to \
             remove. Gate the SUPPRESSION on the collector, never the proof."
        );
    }
}

/// gc-common w6-a (`common-b-proposal-dedupe-the-initiator-root-set.md`):
/// the step-10b skip is scoped to one scan and restored on every exit,
/// unwinding included -- a skip left armed would drop every thread mirror
/// from a later plain `collect_roots` on this thread whose caller does NOT
/// append the registry.
#[test]
fn the_registry_mirror_skip_is_scoped_and_restored() {
    let armed = || REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(std::cell::Cell::get);
    assert!(!armed());
    {
        let _g = RegistryMirrorsAppended(
            REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(|c| c.replace(true)),
        );
        assert!(armed());
    }
    assert!(!armed());
    let unwound = std::panic::catch_unwind(|| {
        let _g = RegistryMirrorsAppended(
            REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(|c| c.replace(true)),
        );
        panic!("a root scan that unwinds");
    });
    assert!(unwound.is_err());
    assert!(!armed(), "an unwinding scan must not leave the skip armed");
}

/// gc-common w6-a: the only caller of the step-10b skip appends the
/// registry's snapshots -- the list that carries the thread mirrors the
/// skip leaves out -- straight after the scan. A second caller, or an
/// append that moved away, would drop every alive thread's mirror from
/// the root set.
#[test]
fn the_registry_appended_scan_is_always_followed_by_the_registry_append() {
    let src = concat!(include_str!("../../runtime/interpreter/gc_and_alloc/mod.rs"), "\n", include_str!("../../runtime/interpreter/gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../../runtime/interpreter/gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../../runtime/interpreter/gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../../runtime/interpreter/gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../../runtime/interpreter/gc_and_alloc/reference_round.rs"), "\n", include_str!("../../runtime/interpreter/gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../../runtime/interpreter/gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../../runtime/interpreter/gc_and_alloc/g1_force_full_cycle.rs"));
    let calls: Vec<usize> = src
        .match_indices("collect_roots_registry_appended(")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "`collect_roots_registry_appended` must have exactly one caller, \
         `run_collection_pause`; audit any new one for the registry append"
    );
    let after = &src[calls[0]..];
    let append = after
        .find("thread_registry.collect_all_root_snapshots()")
        .expect("the registry append after the scan");
    assert!(
        after[..append].lines().count() <= 3,
        "the registry snapshots must be appended right after the scan that \
         skipped their mirrors"
    );
}

/// gcd d2/g (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`): the published counts
/// are the occurrences inside the precise sections only, duplicates
/// counted; a section that does not describe the list publishes nothing.
#[test]
fn gcd_d2g_precise_counts_cover_only_the_precise_sections() {
    // SAFETY: never dereferenced; only the addresses are counted.
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    // [frame word, static, static(dup), frame word, string, mirror, jni]
    let roots = vec![
        at(0x7100),
        at(0x7200),
        at(0x7200),
        at(0x7300),
        at(0x7400),
        at(0x7500),
        at(0x7600),
    ];
    cratonvm_gc::gen_heap::clear_precise_root_values();
    publish_precise_root_counts(&roots, &[(1, 3), (4, 5), (5, 6), (6, 7)]);
    let counts = cratonvm_gc::gen_heap::take_precise_root_values().expect("published");
    assert_eq!(counts.get(&0x7200), Some(&2), "two static slots name it");
    assert_eq!(counts.get(&0x7400), Some(&1));
    assert_eq!(counts.get(&0x7500), Some(&1));
    assert_eq!(counts.get(&0x7600), Some(&1));
    assert_eq!(counts.get(&0x7100), None, "outside every precise section");
    assert_eq!(counts.get(&0x7300), None);

    publish_precise_root_counts(&roots, &[(1, 3), (4, 5), (5, 6), (6, 9)]);
    assert!(
        cratonvm_gc::gen_heap::take_precise_root_values().is_none(),
        "a range past the list publishes nothing"
    );
}

/// gch h3/fd (d2g): `sections` is a slice; a fifth and later section is
/// counted like the four tables, and an occurrence outside every section
/// stays uncounted.
#[test]
fn gch_h3fd_a_fifth_section_is_counted_beside_the_four_tables() {
    // SAFETY: never dereferenced; only the addresses are counted.
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    // [extra section, uncounted twin, static, string, mirror, jni]
    let roots = vec![
        at(0x8100),
        at(0x8100),
        at(0x8200),
        at(0x8300),
        at(0x8400),
        at(0x8500),
    ];
    cratonvm_gc::gen_heap::clear_precise_root_values();
    publish_precise_root_counts(&roots, &[(2, 3), (3, 4), (4, 5), (5, 6), (0, 1)]);
    let counts = cratonvm_gc::gen_heap::take_precise_root_values().expect("published");
    assert_eq!(
        counts.get(&0x8100),
        Some(&1),
        "the sectioned entry counts once; its twin stays uncounted"
    );
    assert_eq!(counts.get(&0x8200), Some(&1));
}
