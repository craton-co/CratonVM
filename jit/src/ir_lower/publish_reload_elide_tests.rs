//! JIT rx27 wave 9, lane regalloc (`CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE`):
//! a value's `MOV RAX, reg` read right behind its own register publish
//! `MOV reg, RAX` is not emitted. On the caller-saved lane's pressure loop,
//! executed.

use super::edge_loop_fixture::{edge_ref, lower_edge_loop, run};

const SWITCH: &str = "CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE";

/// `REX.W 89 /r` register-to-register moves as `(dst, src)`, in order, with
/// their offsets (the encoding `emit_mov_reg_reg64_raw` writes: `reg` is the
/// source, `r/m` the destination).
fn reg_movs(code: &[u8]) -> Vec<(usize, u8, u8)> {
    (2..code.len())
        .filter(|&p| code[p - 2] & 0xFA == 0x48 && code[p - 1] == 0x89 && code[p] & 0xC0 == 0xC0)
        .map(|p| {
            let rex = code[p - 2];
            let src = ((code[p] >> 3) & 7) | (((rex >> 2) & 1) << 3);
            let dst = (code[p] & 7) | ((rex & 1) << 3);
            (p - 2, dst, src)
        })
        .collect()
}

/// Adjacent `MOV reg, RAX` / `MOV RAX, reg` pairs: a publish and its
/// immediate read back.
fn publish_reload_pairs(code: &[u8]) -> usize {
    reg_movs(code)
        .windows(2)
        .filter(|w| {
            let ((a, d0, s0), (b, d1, s1)) = (w[0], w[1]);
            b == a + 3 && s0 == 0 && d0 != 0 && d1 == 0 && s1 == d0
        })
        .count()
}

/// Under the switch the loop has no more publish/read-back pairs than
/// without it, is no longer, and every answer is the reference's. Both
/// counts are printed for the host run.
#[test]
fn rx27w9_a_read_behind_its_publish_is_not_emitted() {
    let off = lower_edge_loop(&[(SWITCH, Some("0"))]);
    let on = lower_edge_loop(&[(SWITCH, Some("1"))]);
    let (p_off, p_on) = (
        publish_reload_pairs(off.code_bytes()),
        publish_reload_pairs(on.code_bytes()),
    );
    let (l_off, l_on) = (off.code_bytes().len(), on.code_bytes().len());
    eprintln!("[rx27w9] publish/read pairs off={p_off} on={p_on}; bytes off={l_off} on={l_on}");
    assert!(p_on <= p_off, "no new pair ({p_on} vs {p_off})");
    assert!(l_on <= l_off, "the body does not grow ({l_on} vs {l_off})");
    if p_off > 0 {
        assert!(
            p_on < p_off,
            "a pair the loop has is skipped ({p_on} vs {p_off})"
        );
    }
    for n in [-2i64, 0, 1, 2, 3, 10, 57, 1000, 100_000] {
        assert_eq!(run(&off, n), edge_ref(n), "f({n}) off");
        assert_eq!(run(&on, n), edge_ref(n), "f({n}) on");
    }
}

/// The scanner on hand-written bytes.
#[test]
fn rx27w9_the_scanner() {
    // mov rbx,rax; mov rax,rbx; mov r12,rax; mov [rbp-8],rax; mov rax,r12; mov rcx,rax
    let code = [
        0x48, 0x89, 0xC3, 0x48, 0x89, 0xD8, 0x49, 0x89, 0xC4, 0x48, 0x89, 0x45, 0xF8, 0x4C, 0x89,
        0xE0, 0x48, 0x89, 0xC1,
    ];
    let movs = reg_movs(&code);
    assert_eq!(
        movs.iter().map(|&(_, d, s)| (d, s)).collect::<Vec<_>>(),
        vec![(3, 0), (0, 3), (12, 0), (0, 12), (1, 0)]
    );
    assert_eq!(
        publish_reload_pairs(&code),
        1,
        "the r12 pair has a store between"
    );
}

/// The 2026-10-29 regalloc flip: the four switches are default ON, `=0`
/// kills each one, and `=1` keeps it on.
#[test]
fn rx27w9_the_regalloc_flip_is_default_on() {
    let readers: [(&str, fn() -> bool); 4] = [
        (
            "CRATONVM_JIT_IR_AND_ZERO_TEST_BRANCH",
            super::ir_and_zero_test_branch_enabled,
        ),
        (
            "CRATONVM_JIT_IR_AND_ZERO_TEST_GUARD",
            super::ir_and_zero_test_guard_enabled,
        ),
        (
            "CRATONVM_JIT_IR_OPERAND_IN_PLACE",
            super::ir_operand_in_place_enabled,
        ),
        (SWITCH, super::ir_publish_reload_elide_enabled),
    ];
    for (key, read) in readers {
        for (v, want) in [(None, true), (Some("0"), false), (Some("1"), true)] {
            cratonvm_types::flags::with_thread_overrides(&[(key, v)], || {
                assert_eq!(read(), want, "{key}={v:?}")
            });
        }
    }
}
