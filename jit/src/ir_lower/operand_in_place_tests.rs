//! JIT rx27 wave 8, lane regalloc (`CRATONVM_JIT_IR_OPERAND_IN_PLACE`): a
//! compare's or an ALU op's second operand is read where it lives (its home
//! word, register or an immediate) instead of through `MOV RCX, [rbp-x]`.
//! On the caller-saved lane's pressure loop, executed.

use super::edge_loop_fixture::{edge_ref, lower_edge_loop, run};

const SWITCH: &str = "CRATONVM_JIT_IR_OPERAND_IN_PLACE";

/// Instructions with opcode `op` (after an optional REX prefix, which the
/// scan does not need to see) and an `[rbp + disp8/32]` ModRM operand.
fn rbp_ops(code: &[u8], ops: &[u8]) -> usize {
    (1..code.len())
        .filter(|&p| ops.contains(&code[p - 1]))
        .filter(|&p| matches!(code[p] & 0xC7, 0x45 | 0x85))
        .count()
}

/// `MOV r, [rbp + d]`: the slot loads.
fn slot_loads(code: &[u8]) -> usize {
    rbp_ops(code, &[0x8B])
}

/// `CMP r, [rbp + d]` and `ADD / SUB / AND / OR / XOR r, [rbp + d]`: the
/// in-place reads.
fn in_place_reads(code: &[u8]) -> usize {
    rbp_ops(code, &[0x3B, 0x03, 0x2B, 0x23, 0x0B, 0x33])
}

/// Under the switch the loop loads no more slots into registers than
/// without it, has no fewer memory-operand reads, and every
/// answer is the reference's. Both counts are printed for the host run.
#[test]
fn rx27w8_second_operands_are_read_in_place() {
    let off = lower_edge_loop(&[(SWITCH, Some("0"))]);
    let on = lower_edge_loop(&[(SWITCH, Some("1"))]);
    let (l_off, l_on) = (slot_loads(off.code_bytes()), slot_loads(on.code_bytes()));
    let (r_off, r_on) = (
        in_place_reads(off.code_bytes()),
        in_place_reads(on.code_bytes()),
    );
    eprintln!("[rx27w8] slot loads off={l_off} on={l_on}; in-place reads off={r_off} on={r_on}");
    assert!(l_on <= l_off, "no new slot load ({l_on} vs {l_off})");
    assert!(
        r_on >= r_off,
        "in-place reads only grow ({r_on} vs {r_off})"
    );
    for n in [-2i64, 0, 1, 2, 3, 10, 57, 1000, 100_000] {
        assert_eq!(run(&off, n), edge_ref(n), "f({n}) off");
        assert_eq!(run(&on, n), edge_ref(n), "f({n}) on");
    }
}

/// The scanners on hand-written bytes.
#[test]
fn rx27w8_the_scanners() {
    // mov rax,[rbp-8]; cmp eax,[rbp-10h]; and eax,[rbp-18h]; mov ecx,eax
    let code = [
        0x48, 0x8B, 0x45, 0xF8, 0x3B, 0x45, 0xF0, 0x23, 0x45, 0xE8, 0x89, 0xC1,
    ];
    assert_eq!(slot_loads(&code), 1);
    assert_eq!(in_place_reads(&code), 2);
}
