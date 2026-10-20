// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 10, lane ffmeintr: source witnesses for the four pages
//! of the lane. Each property is observable only with a live VM under load
//! (a pause racing a native, a blocked peer read by the helper-window pass),
//! which no test in this crate can stage; the probes are
//! `R18FfmeintrLeaveStarvation.java` and `R17Ffm3EintrRace.java`.

fn body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` exists"));
    let rest = &src[start..];
    let end = rest.find("\n    }\n").map_or(rest.len(), |e| e + 7);
    &rest[..end]
}

/// `r17-native-alloc-gc-point-rule-differs-between-api-and-moving-mock`: each
/// of the seven no-GC allocation doors opens the debug-build check before
/// anything else it does.
#[test]
fn every_no_gc_allocation_door_opens_the_check_first() {
    let src = include_str!("../src/vm/vm_exec.rs").replace("\r\n", "\n");
    for door in [
        "alloc_object",
        "try_alloc_object",
        "new_array",
        "try_new_array",
        "new_ref_array",
        "try_new_ref_array",
        "create_string",
    ] {
        let b = body(&src, &format!("\n    fn {door}("));
        let guard = format!("native_alloc_no_gc_point(\"{door}\", self.shared)");
        let at = b
            .find(&guard)
            .unwrap_or_else(|| panic!("{door} opens the check"));
        let first_self_call = b
            .find("self.native")
            .or_else(|| b.find("self.shared.mem.heap"));
        if let Some(call) = first_self_call {
            assert!(
                at < call,
                "{door}: the check opens before the door allocates"
            );
        }
    }
    // Zero release cost: the flag is read only behind `cfg!(debug_assertions)`.
    let helper = body(&src, "fn native_alloc_no_gc_point<'a>(");
    let dbg = helper
        .find("cfg!(debug_assertions)")
        .expect("debug builds only");
    let flag = helper
        .find("CRATONVM_DBG_NATIVE_ALLOC_NO_GC_POINT")
        .expect("behind its debug switch");
    assert!(dbg < flag);
}

/// `r17-ffm3-eintr-race-rare-silent-hang-in-the-battery`: the release of a
/// claim hands a waiting leave over (`CLAIMED_LEAVING -> OFF`), and only the
/// thread itself marks itself leaving.
#[test]
fn the_claim_release_hands_a_waiting_leave_over() {
    let src = include_str!("../src/threading/jvm_thread.rs").replace("\r\n", "\n");
    let release = body(
        &src,
        "    pub fn release_lazy_native_claim(&self) -> bool {",
    );
    assert!(release.contains("LAZY_NATIVE_CLAIMED_LEAVING,\n                LAZY_NATIVE_OFF,"));
    let leave = body(&src, "    fn leave_lazy_native_slow(&self) -> bool {");
    assert!(leave.contains(
        "LAZY_NATIVE_CLAIMED,\n                            LAZY_NATIVE_CLAIMED_LEAVING,"
    ));
    let claim = body(
        &src,
        "    pub fn try_claim_lazy_native(&self) -> Option<(usize, usize)> {",
    );
    assert!(
        claim.contains("!= LAZY_NATIVE_IN"),
        "a claim takes only a thread still in C"
    );
}

/// The helper-window pass reads a blocked peer's published band before it
/// parks it, and the unparked read holds the band while it copies.
#[test]
fn the_helper_window_pass_takes_no_band_lock_inside_a_parked_window() {
    let src = include_str!("../src/jit/xt_root_scan.rs").replace("\r\n", "\n");
    let linux = src
        .split("// Linux x86-64 implementation (signal rendezvous)\n")
        .nth(1)
        .expect("the Linux arm's banner");
    let hw = linux
        .split("pub fn helper_window_pass<F>(")
        .nth(1)
        .expect("linux helper_window_pass");
    let preread = hw
        .find("let preread = helper_band_preread(tid);")
        .expect("pre-read");
    let arm = hw.find("arm_slot(tid, true)").expect("arms a helper slot");
    assert!(preread < arm, "the band is read before the park");
    assert!(hw.contains("snapshot_parked_slot_with(slot, &regions, &mut band, preread)"));
    let hold = hw.find("state.try_hold_blocked_wait_band()").expect("hold");
    let copy = hw
        .find("copy_peer_band(lo, end, clamped, &mut band)")
        .expect("copy");
    let release = hw
        .find("state.release_blocked_wait_band(lo)")
        .expect("release");
    assert!(
        hold < copy && copy < release,
        "the copy runs under the hold"
    );
}

/// Wave 10b (`CRATONVM_FFM_LAZY_SIGNAL_GATE`): the Linux take-over pass reads
/// the kernel, then gates the peer's lazy in-native entry, and only then arms
/// a slot and signals; a peer found in lazy C is skipped.
#[test]
fn the_linux_take_over_gates_lazy_entry_before_it_signals() {
    let src = include_str!("../src/jit/xt_root_scan.rs").replace("\r\n", "\n");
    let linux = src
        .split("// Linux x86-64 implementation (signal rendezvous)\n")
        .nth(1)
        .expect("the Linux arm's banner");
    let pass = linux
        .split("pub fn take_over_pass<F>(")
        .nth(1)
        .and_then(|s| s.split("pub fn resume(").next())
        .expect("linux take_over_pass");
    let syscall = pass
        .find("takeover_signal_skips_peer_in_syscall(peer_syscall_state(tid)")
        .expect("the kernel read");
    let gate = pass
        .find("state.try_gate_takeover_signal()")
        .expect("the gate");
    let arm = pass.find("arm_slot(tid, false)").expect("arms a slot");
    assert!(syscall < gate && gate < arm);
    assert!(pass.contains("TakeoverSignalGate::InNative => {\n                        lazy_skipped += 1;\n                        continue;"));
    assert!(pass.contains("state.ungate_takeover_signal()"));
}
