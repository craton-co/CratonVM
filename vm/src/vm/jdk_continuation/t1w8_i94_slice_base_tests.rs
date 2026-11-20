use super::*;

#[inline(never)]
fn callee_local_addr() -> usize {
    let probe = 0u64;
    std::hint::black_box(&probe) as *const u64 as usize
}

#[test]
fn t1w8_i94_the_slice_base_is_above_every_callee_word() {
    let base = slice_base_here!();
    assert_eq!(base % 8, 0, "word-aligned");
    let call_sp = base - SLICE_CALLEE_HOME_BYTES;
    // A callee's return address sits at `call_sp - 8` (x64; AArch64 keeps
    // it in the frame record below `call_sp`): its locals are lower still.
    let callee = callee_local_addr();
    assert!(callee + 16 <= call_sp, "callee {callee:#x} vs call sp {call_sp:#x}");
    // The old base: a returned callee's local, below the next callee's
    // return-address slot (x64), which therefore landed on the carrier's
    // side.
    let old = slice_base_sp();
    #[cfg(target_arch = "x86_64")]
    assert!(old < call_sp - 8, "old {old:#x} vs call sp {call_sp:#x}");
    assert!(old < base);
}
