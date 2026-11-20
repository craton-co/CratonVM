use super::*;

fn read(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path)
        .expect("source is readable")
        .replace("\r\n", "\n")
}

/// The text of the item starting at `sig`, up to its column-0 `}`.
fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

fn at(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

// `fake` addresses: never dereferenced, only pointer-map keys / values.
fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; a pointer-map key / value only.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Finding 1: the JDK continuation's yield retires its TLAB BEFORE the
/// deposit raises `in_blocked_region` (no blocked thread keeps a TLAB).
#[test]
fn gci_i11vt_jdk_yield_retires_the_tlab_before_the_deposit() {
    let src = read("src/vm/jdk_continuation.rs");
    let slice = body(&src, "fn run_slice(");
    let arm = &slice[at(
        &slice,
        "VmError::ContinuationYield { .. })) = &result",
        "run_slice yield arm",
    )..];
    let retire = at(arm, "thread.tlab.retire();", "yield-arm retire");
    let deposit = at(arm, ".deposit_root_snapshot();", "yield-arm deposit");
    let handoff = at(arm, ".deposit_continuation(", "yield-arm hand-off");
    assert!(
        retire < deposit && deposit < handoff,
        "run_slice's yield arm must retire the TLAB before its deposit raises \
         in_blocked_region (retire@{retire} deposit@{deposit} handoff@{handoff})"
    );
}

/// Finding 1, the bound path (`CRATONVM_VT_CONTINUATIONS=0`).
#[test]
fn gci_i11vt_bound_yield_retires_the_tlab_before_the_deposit() {
    let src = read("src/vm/vm_exec/diagnose_sync_on_value_based_class.rs");
    let resume = body(&src, "fn resume_virtual_continuation(");
    let arm = &resume[at(
        &resume,
        "VmError::ContinuationYield { wake_after_nanos })) =",
        "resume_virtual_continuation yield arm",
    )..];
    let retire = at(arm, "thread.tlab.retire();", "bound yield-arm retire");
    let deposit = at(arm, ".deposit_root_snapshot();", "bound yield-arm deposit");
    let handoff = at(arm, ".suspend_runtime(", "bound yield-arm hand-off");
    assert!(
        retire < deposit && deposit < handoff,
        "resume_virtual_continuation's yield arm must retire the TLAB before its \
         deposit (retire@{retire} deposit@{deposit} handoff@{handoff})"
    );
}

/// Finding 2: a remount reads the registry's (current) mirror only after
/// the drain applied the yield-keyed fixup to the yield-time one.
#[test]
fn gci_i11vt_jdk_remount_reads_the_mirror_after_the_drain() {
    let src = read("src/vm/jdk_continuation.rs");
    let slice = body(&src, "fn run_slice(");
    let drain = at(&slice, ".check_post_block_gc();", "remount drain");
    let mirror = at(&slice, "registry.java_thread_obj(tid)", "mirror refresh");
    assert!(
        drain < mirror,
        "run_slice must refresh java_thread_obj from the registry after \
         check_post_block_gc (drain@{drain} mirror@{mirror})"
    );
}

/// Finding 2's hazard, shown on the shared remap the drain uses: the
/// blocked chain is keyed by YIELD-TIME addresses, so under a sliding
/// compactor's `A -> B, B -> C` it forwards the yield-time mirror `A` to
/// `B` and an already-current `B` on to `C` -- another root's object.
#[test]
fn gci_i11vt_the_blocked_chain_moves_a_current_address_twice() {
    let mut chain = cratonvm_types::PointerMap::default();
    chain.insert(0xA000, 0xB000);
    chain.insert(0xB000, 0xC000);
    let mut thread = JvmThread::new(ThreadId(7), "gci-i11vt");
    thread.java_thread_obj = Some(fake(0xA000));
    crate::memory::gc::remap_thread_single_slots(&mut thread, &chain);
    assert_eq!(
        thread.java_thread_obj.map(|o| o.as_ptr() as usize),
        Some(0xB000),
        "the yield-time mirror is forwarded once"
    );
    thread.java_thread_obj = Some(fake(0xB000));
    crate::memory::gc::remap_thread_single_slots(&mut thread, &chain);
    assert_eq!(
        thread.java_thread_obj.map(|o| o.as_ptr() as usize),
        Some(0xC000),
        "a current address fed to the chain lands on another root (why the \
         refresh follows the drain)"
    );
}

/// Finding 3: a debugger reads an unmounted continuation's reference
/// locals through its yield deposit's `slot_origins`, never the raw
/// (yield-time) slot.
#[test]
fn gci_i11vt_unmounted_frame_readers_go_through_slot_origins() {
    let src = read("src/runtime/interpreter/park_for_debugger_under.rs");
    let work = body(&src, "pub(crate) fn with_unmounted_frames<R>(");
    assert!(
        work.contains("state: Some(&*thread.gc_block_state)") && work.contains("unmounted: true"),
        "with_unmounted_frames must hand current_reference the continuation's GcBlockState"
    );
    assert!(
        !work.contains("state: None"),
        "with_unmounted_frames must not read raw slots"
    );
    let publish = body(&src, "fn publish_unmounted_frames(");
    assert!(
        publish.contains(".slot_origins") && publish.contains("is_object_address(cur)"),
        "publish_unmounted_frames must publish reference locals through slot_origins"
    );
    let unmounted = body(&src, "impl BlockedFrames<'_> {");
    assert!(
        unmounted.contains("self.unmounted\n"),
        "BlockedFrames::unmounted must read its own flag, not state.is_none()"
    );
}

/// Item 4: both ends of a continuation (its slice's end and the no-runner
/// failure of a first slice) go through `end_manager_record`, which reaps
/// only under the per-VM switch and never while JDWP is configured; VM
/// init reads the switch once, into the manager.
#[test]
fn gci_i11vt_continuation_ends_reap_through_the_per_vm_switch() {
    let src = read("src/vm/jdk_continuation.rs");
    let end = body(&src, "fn end_manager_record(");
    let terminate = at(&end, "manager.terminate(tid.0);", "terminate");
    let gate = at(&end, "manager.reap_terminated()", "per-VM switch");
    let forget = at(&end, "manager.forget_terminated(tid.0);", "reap");
    assert!(terminate < gate && gate < forget);
    assert!(end.contains("debugger_observes_locals(shared)"));
    let slice = body(&src, "fn run_slice(");
    assert!(
        slice.contains("end_manager_record(shared, tid);")
            && !slice.contains("virtual_thread_manager.terminate("),
        "run_slice's end must go through end_manager_record"
    );
    let init = read("src/vm/vm_init/real_jdk_to_array_typed.rs");
    assert!(
        init.contains(".with_reap_terminated(true)"),
        "VM init enables terminated continuation reaping"
    );
}

/// Item 8: a first slice that found no runner is retired (registry entry
/// dead, addresses withdrawn between pauses, manager record ended) and
/// dropped -- never deposited as a zombie.
#[test]
fn gci_i11vt_a_never_mounted_first_slice_is_retired_not_deposited() {
    let src = read("src/vm/jdk_continuation.rs");
    let runner = body(&src, "fn run_on_runner(");
    let arm = &runner[at(&runner, "Err(message) => {", "no-runner arm")..];
    let first = at(arm, "if first {", "first-slice branch");
    let census = at(arm, "run_if_no_stw_requested(", "census-serialized retire");
    let withdraw = at(arm, "registry.clear_tlab_addr(tid);", "address withdrawal");
    let dead = at(arm, "registry.mark_dead(tid);", "mark_dead");
    let ended = at(arm, "end_manager_record(shared, tid);", "manager record");
    let dropped = at(arm, "drop(thread);", "drop");
    let deposit = at(arm, ".deposit_continuation(", "resumed-slice deposit");
    assert!(
        first < census
            && census < withdraw
            && withdraw < dead
            && dead < ended
            && ended < dropped
            && dropped < deposit,
        "the first-slice branch retires and returns before the deposit"
    );
}

/// Item 9: `Thread.print` (`thread_snapshots`) leaves JDK continuations'
/// entries out, as HotSpot 25 lists only platform threads there.
#[test]
fn gci_i11vt_thread_print_leaves_jdk_continuations_out() {
    let init = read("src/vm/vm_init/class_store_cut_with.rs");
    let snapshots = body(&init, "    fn thread_snapshots(&self)");
    assert!(
        snapshots.contains("vtm.is_jdk_continuation(tid.0)")
            && snapshots.contains(".filter(|e| e.alive && !continuation_entry(e.thread_id))"),
        "thread_snapshots must filter JDK continuation entries"
    );
}

/// Item 10: the one check both interpreter->compiled entries pass, before
/// they install the JIT thread; and what it asks.
#[test]
fn gci_i11vt_compiled_entries_refuse_a_continuation() {
    let bridge = format!(
        "{}\n{}",
        read("src/runtime/interpreter/jit_bridge/mint_ldc_slots_at_offer.rs"),
        read("src/runtime/interpreter/jit_bridge/try_osr.rs")
    );
    for door in ["fn run_jit_body_raw(", "fn try_osr("] {
        let fun = body(&bridge, door);
        let check = at(&fun, "assert_no_compiled_continuation(thread);", door);
        let enter = at(&fun, "crate::jit::helpers::set_jit_thread(thread)", door);
        assert!(check < enter, "{door}: the check precedes the entry");
    }
    let mut thread = JvmThread::new(ThreadId(11), "gci-i11vt-kind");
    assert!(!crate::runtime::interpreter::jit_bridge::thread_is_continuation(&thread));
    thread.kind = crate::threading::ThreadKind::Virtual;
    assert!(crate::runtime::interpreter::jit_bridge::thread_is_continuation(&thread));
}
