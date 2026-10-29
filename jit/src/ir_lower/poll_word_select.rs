// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 1 (lane LOOPCG; page
//! `docs/known-issues/jit/jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md`):
//! the IR safepoint poll under `CRATONVM_JIT_POLL_THREAD_WORD`, with the poll
//! byte's address chosen into R11 first and tested once.
//!
//! # Shape
//!
//! The two-test form `Lowerer::emit_safepoint_poll` emitted since interpreter
//! round i6 wave 119:
//!
//! ```text
//!   MOV R11, <seg>:[disp32] ; TEST R11, R11 ; JZ .global
//!   TEST byte [R11], 0xFF   ; JMP .tested
//! .global:
//!   TEST byte [rip + flag], 0xFF
//! .tested:
//!   Jcc slow
//! ```
//!
//! runs six instructions on its hot path (mirror set): the load, the null
//! test, the untaken `JZ`, the byte test, the taken `JMP`, the poll's `Jcc`.
//! This form:
//!
//! ```text
//!   MOV R11, <seg>:[disp32] ; TEST R11, R11 ; JNZ .have   (rel8)
//!   LEA R11, [rip + flag]                       ; or MOV R11, imm64 flag
//! .have:
//!   TEST byte [R11], 0xFF
//!   Jcc slow
//! ```
//!
//! runs five (one taken branch either way) and is shorter.
//!
//! # Why it is the same poll
//!
//! Both arms end on `TEST byte [X], 0xFF` with X the byte the two-test form
//! tests on the same arm: the mirror's word when it is non-zero, the baked
//! process flag when it is zero. The flags the following `Jcc` reads are that
//! test's, as before. R11 is the poll's scratch in both forms (the slow path
//! loads its own arguments), and every jump is local to the sequence, so a
//! stress countdown landing on the `Jcc`, the forced-exit patch of that `Jcc`
//! and the outlined slow path see exactly what they saw.

/// `CRATONVM_JIT_IR_POLL_WORD_SELECT` (j27 wave 1, lane LOOPCG; performance,
/// **default ON**, `=0` restores the two-test form): emit the select-then-test
/// thread-word poll. Only consulted when `CRATONVM_JIT_POLL_THREAD_WORD` is in
/// effect. No `static`: a snapshot lookup per emitted poll.
pub(super) fn poll_word_select_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_POLL_WORD_SELECT")
}

/// The bytes of the select-then-test poll prefix (everything up to, not
/// including, the poll's `Jcc`), for a sequence that starts at absolute
/// address `here`. `seg_prefix` is the TLS segment override, `tls_disp` the
/// mirror slot's displacement, `flag_addr` the baked process flag. With
/// `rip_ok` and the flag in RIP reach the fallback is a 7-byte `LEA`, else a
/// 10-byte `MOV R11, imm64`.
pub(super) fn poll_word_select_bytes(
    here: usize,
    seg_prefix: u8,
    tls_disp: u32,
    flag_addr: usize,
    rip_ok: bool,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    b.push(seg_prefix);
    b.extend_from_slice(&[0x4C, 0x8B, 0x1C, 0x25]); // MOV R11, <seg>:[disp32]
    b.extend_from_slice(&tls_disp.to_le_bytes());
    b.extend_from_slice(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    // The fallback starts after the two-byte `JNZ rel8`.
    const LEA_LEN: usize = 7;
    let lea_at = here.wrapping_add(b.len() + 2);
    let rip = if rip_ok {
        super::rip_disp32(lea_at, LEA_LEN, flag_addr)
    } else {
        None
    };
    let mut fallback: Vec<u8> = Vec::with_capacity(10);
    match rip {
        Some(disp) => {
            fallback.extend_from_slice(&[0x4C, 0x8D, 0x1D]); // LEA R11, [rip + disp32]
            fallback.extend_from_slice(&disp.to_le_bytes());
        }
        None => {
            fallback.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
            // Cast: a data address.
            fallback.extend_from_slice(&(flag_addr as u64).to_le_bytes());
        }
    }
    b.push(0x75); // JNZ rel8 .have
    // Cast: 7 or 10, below 0x80.
    b.push(fallback.len() as u8);
    b.extend_from_slice(&fallback);
    b.extend_from_slice(&[0x41, 0xF6, 0x03, 0xFF]); // .have: TEST byte [R11], 0xFF
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEG: u8 = 0x65;
    const DISP: u32 = 0x1480;

    /// Offset of the `JNZ rel8` in the sequence: prefix + MOV (4 + 4) + TEST (3).
    const JNZ_AT: usize = 1 + 4 + 4 + 3;

    #[test]
    fn j27_loopcg_the_mirror_load_and_null_test_come_first() {
        let b = poll_word_select_bytes(0x1000, SEG, DISP, 0x2000, true);
        assert_eq!(b[0], SEG);
        assert_eq!(&b[1..5], &[0x4C, 0x8B, 0x1C, 0x25]);
        assert_eq!(&b[5..9], &DISP.to_le_bytes());
        assert_eq!(&b[9..12], &[0x4D, 0x85, 0xDB]);
        assert_eq!(b[JNZ_AT], 0x75, "JNZ rel8 over the fallback");
    }

    /// In reach: the fallback is a `LEA` whose displacement names the flag
    /// from the end of the `LEA`, and the `JNZ` lands on the one byte test.
    #[test]
    fn j27_loopcg_in_reach_the_fallback_is_a_rip_lea_of_the_flag() {
        let here = 0x7000_0000usize;
        let flag = here + 0x10_0000;
        let b = poll_word_select_bytes(here, SEG, DISP, flag, true);
        let lea = JNZ_AT + 2;
        assert_eq!(b[JNZ_AT + 1], 7);
        assert_eq!(&b[lea..lea + 3], &[0x4C, 0x8D, 0x1D]);
        let disp = i32::from_le_bytes([b[lea + 3], b[lea + 4], b[lea + 5], b[lea + 6]]);
        let next = here + lea + 7;
        assert_eq!((next as i64 + i64::from(disp)) as usize, flag);
        let have = JNZ_AT + 2 + usize::from(b[JNZ_AT + 1]);
        assert_eq!(&b[have..], &[0x41, 0xF6, 0x03, 0xFF]);
        assert_eq!(b.len(), have + 4);
    }

    /// Out of reach, or with the RIP poll killed: `MOV R11, imm64 flag`.
    #[test]
    fn j27_loopcg_out_of_reach_or_rip_off_the_fallback_is_an_imm64() {
        let here = 0x1000usize;
        let far = 0x7FFF_0000_0000usize;
        for (flag, rip_ok) in [(far, true), (here + 0x100, false)] {
            let b = poll_word_select_bytes(here, SEG, DISP, flag, rip_ok);
            assert_eq!(b[JNZ_AT + 1], 10);
            let mov = JNZ_AT + 2;
            assert_eq!(&b[mov..mov + 2], &[0x49, 0xBB]);
            let mut imm = [0u8; 8];
            imm.copy_from_slice(&b[mov + 2..mov + 10]);
            assert_eq!(u64::from_le_bytes(imm), flag as u64);
            assert_eq!(&b[mov + 10..], &[0x41, 0xF6, 0x03, 0xFF]);
        }
    }

    #[test]
    fn j27_loopcg_poll_word_select_is_default_on_with_a_zero_kill() {
        const KEY: &str = "CRATONVM_JIT_IR_POLL_WORD_SELECT";
        assert!(cratonvm_types::flags::with_thread_overrides(
            &[(KEY, None)],
            poll_word_select_enabled
        ));
        assert!(!cratonvm_types::flags::with_thread_overrides(
            &[(KEY, Some("0"))],
            poll_word_select_enabled
        ));
    }
}
