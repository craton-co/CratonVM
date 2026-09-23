// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, `baseline` lane: executed fixtures for the single-pass
//! backend's control-flow and operand-stack fixes. Each method is compiled by
//! `x64::compile` and RUN, and its answer is checked against the value JVMS
//! semantics give — a switch that lands on the wrong arm, or an operand that
//! reads a clobbered home, does not crash; it returns a plausible wrong number.
//!
//! See `docs/internal/jit-review-r9/NOTES-baseline.md` for the defects.

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::compile;
use cratonvm_jit::CompiledMethod;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// No fixture here reaches a runtime helper. The two that the backend may
/// reach on an ordinary path get real no-ops (as `differential.rs` does);
/// everything else stays unwired.
fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::MIN
    }
    JitRuntimeHelpers {
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable as *const () as usize,
        ..Default::default()
    }
}

/// Compile `code` (two padding bytes appended, as the in-crate fixtures do).
fn compile_method(code: &[u8], num_params: usize, max_locals: usize) -> Option<CompiledMethod> {
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
    compile(
        &padded,
        code.len(),
        num_params,
        max_locals,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        None,
    )
}

fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    // SAFETY: every fixture below is a self-contained static method over its
    // int/float arguments that reaches no runtime helper; the body was
    // compiled by this test for exactly these argument kinds.
    unsafe { m.try_call(args).expect("test JIT call") }
}

fn push_i32(code: &mut Vec<u8>, v: i32) {
    code.extend_from_slice(&v.to_be_bytes());
}

/// `int f(int k) { switch (k) { <arms> default: return -1; } }` as a
/// `tableswitch` over `[low, high]`. `arm(i)` is `Some(v)` for a slot that
/// returns `v`, `None` for a hole that goes to the default. Returns the code
/// and a host oracle.
fn tableswitch_method(
    low: i32,
    high: i32,
    arm: impl Fn(i32) -> Option<i32>,
) -> (Vec<u8>, impl Fn(i32) -> i32) {
    let count = (i64::from(high) - i64::from(low) + 1) as usize;
    let mut code = vec![0x1a]; // 0: iload_0
    let switch_pc = code.len();
    code.push(0xaa);
    while code.len() % 4 != 0 {
        code.push(0);
    }
    let table_end = code.len() + 12 + count * 4;
    // Each live arm is `sipush v; ireturn` (4 bytes); the default follows.
    let live: Vec<(i32, i32)> = (0..count as i32)
        .filter_map(|i| arm(low + i).map(|v| (low + i, v)))
        .collect();
    let default_pc = table_end + live.len() * 4;
    let rel = |t: usize| t as i32 - switch_pc as i32;
    push_i32(&mut code, rel(default_pc));
    push_i32(&mut code, low);
    push_i32(&mut code, high);
    let mut next_arm = table_end;
    for i in 0..count as i32 {
        if arm(low + i).is_some() {
            push_i32(&mut code, rel(next_arm));
            next_arm += 4;
        } else {
            push_i32(&mut code, rel(default_pc));
        }
    }
    assert_eq!(code.len(), table_end);
    for &(_, v) in &live {
        let v16 = i16::try_from(v).expect("fixture values fit sipush");
        code.push(0x11);
        code.extend_from_slice(&v16.to_be_bytes());
        code.push(0xac);
    }
    code.extend_from_slice(&[0x02, 0xac]); // default: iconst_m1; ireturn
    let oracle = move |k: i32| {
        live.iter()
            .find(|&&(key, _)| key == k)
            .map_or(-1, |&(_, v)| v)
    };
    (code, oracle)
}

/// Same shape as a `lookupswitch` over `pairs` (key, Some(v) | None = default).
fn lookupswitch_method(pairs: &[(i32, Option<i32>)]) -> (Vec<u8>, impl Fn(i32) -> i32) {
    let mut code = vec![0x1a]; // 0: iload_0
    let switch_pc = code.len();
    code.push(0xab);
    while code.len() % 4 != 0 {
        code.push(0);
    }
    let table_end = code.len() + 8 + pairs.len() * 8;
    let live: Vec<(i32, i32)> = pairs
        .iter()
        .filter_map(|&(k, v)| v.map(|v| (k, v)))
        .collect();
    let default_pc = table_end + live.len() * 4;
    let rel = |t: usize| t as i32 - switch_pc as i32;
    push_i32(&mut code, rel(default_pc));
    push_i32(&mut code, pairs.len() as i32);
    let mut next_arm = table_end;
    for &(k, v) in pairs {
        push_i32(&mut code, k);
        if v.is_some() {
            push_i32(&mut code, rel(next_arm));
            next_arm += 4;
        } else {
            push_i32(&mut code, rel(default_pc));
        }
    }
    assert_eq!(code.len(), table_end);
    for &(_, v) in &live {
        let v16 = i16::try_from(v).expect("fixture values fit sipush");
        code.push(0x11);
        code.extend_from_slice(&v16.to_be_bytes());
        code.push(0xac);
    }
    code.extend_from_slice(&[0x02, 0xac]);
    let oracle = move |k: i32| {
        live.iter()
            .find(|&&(key, _)| key == k)
            .map_or(-1, |&(_, v)| v)
    };
    (code, oracle)
}

fn probe_keys(low: i32, high: i32) -> Vec<i32> {
    let mut keys: Vec<i32> = (low.saturating_sub(3)..=high.saturating_add(3)).collect();
    keys.extend_from_slice(&[i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX]);
    keys
}

/// A dense `tableswitch` at `low == 0` takes the jump table, which is the
/// shape whose index used to skip normalisation entirely (`SUB EAX, 0` was
/// omitted, leaving bits 63:32 of the index to the key's producer). The fix
/// emits `MOV EAX, EAX` there; the dispatch must still be exact on every key,
/// including the ones that are negative as an `i32` and therefore huge as an
/// unsigned index.
#[test]
fn a_zero_based_jump_table_dispatches_every_key_exactly() {
    let (code, oracle) = tableswitch_method(0, 7, |k| Some(100 + k));
    let m = compile_method(&code, 1, 1).expect("tableswitch compiles");
    for k in probe_keys(0, 7) {
        assert_eq!(call(&m, &[i64::from(k)]), i64::from(oracle(k)), "key {k}");
    }
    // An int whose upper register half is NOT a sign extension — what an
    // ABI-legal `i32` producer may leave behind. JVMS reads only the int.
    for k in 0..8i32 {
        let dirty = (0x7fff_1234_i64 << 32) | i64::from(k);
        assert_eq!(call(&m, &[dirty]), i64::from(oracle(k)), "dirty key {k}");
    }
    // The normalisation is in the stream: `MOV EAX, EAX` straight before
    // `CMP EAX, imm32` (count 8).
    let bytes = m.code_bytes();
    let want = [0x89, 0xC0, 0x3D, 0x08, 0x00, 0x00, 0x00];
    assert!(
        bytes.windows(want.len()).any(|w| w == want),
        "a low == 0 jump table must zero-extend its index before the bounds check"
    );
}

/// A table whose slots are mostly holes has few LIVE cases, and now takes the
/// compare chain (keyed on the case value itself) instead of the indirect
/// jump. Holes, the edges of the range and far-out keys all reach the default.
#[test]
fn a_table_of_holes_dispatches_like_the_bytecode_says() {
    for (low, high) in [
        (0, 9),
        (-5, 4),
        (1000, 1011),
        (i32::MAX - 7, i32::MAX),
        (i32::MIN, i32::MIN + 6),
    ] {
        let (code, oracle) =
            tableswitch_method(low, high, |k| (k == low || k == high).then_some(7));
        let m = compile_method(&code, 1, 1).expect("tableswitch compiles");
        for k in probe_keys(low, high) {
            assert_eq!(
                call(&m, &[i64::from(k)]),
                i64::from(oracle(k)),
                "[{low}, {high}] key {k}"
            );
        }
    }
}

/// A sorted `lookupswitch` whose default-bound pairs are now dropped before
/// the chain / binary search is built: every listed key, including the ones
/// that name the default, must still answer as JVMS says.
#[test]
fn a_lookupswitch_with_default_bound_pairs_dispatches_exactly() {
    let pairs: Vec<(i32, Option<i32>)> = vec![
        (-1000, Some(1)),
        (-7, None),
        (-3, Some(2)),
        (0, None),
        (5, Some(3)),
        (9, None),
        (40, Some(4)),
        (41, Some(5)),
        (100, None),
        (1 << 20, Some(6)),
    ];
    let (code, oracle) = lookupswitch_method(&pairs);
    let m = compile_method(&code, 1, 1).expect("lookupswitch compiles");
    let mut keys: Vec<i32> = pairs.iter().map(|&(k, _)| k).collect();
    keys.extend(probe_keys(-10, 45));
    for k in keys {
        assert_eq!(call(&m, &[i64::from(k)]), i64::from(oracle(k)), "key {k}");
    }
}

/// `swap` over two frame words must not physically exchange a word a DEEPER
/// entry still reads. `istore_0` with two copies of a register-homed local 0
/// on the stack repoints both at one shared word; the swap below names that
/// word as its lower operand.
///
/// `f(x) = { push x; push x; x = 5; push 1; swap; isub; isub }`
/// = `x - (1 - x)` = `2x - 1`. The exchange handed the deepest entry the `1`
/// and answered `x`.
#[test]
fn swap_does_not_exchange_a_word_a_deeper_entry_still_reads() {
    let code = [
        0x1a, // 0: iload_0
        0x1a, // 1: iload_0
        0x08, // 2: iconst_5
        0x3b, // 3: istore_0
        0x04, // 4: iconst_1
        0x5f, // 5: swap
        0x64, // 6: isub
        0x64, // 7: isub
        0xac, // 8: ireturn
    ];
    let m = compile_method(&code, 1, 1).expect("swap fixture compiles");
    for x in [10i64, 0, -3, 1 << 20] {
        assert_eq!(call(&m, &[x]), 2 * x - 1, "x = {x}");
    }
}

/// `f + (f = 2.0f)` — the stored-to local was also on the stack as a
/// zero-cost `Xmm(home)` entry (Win64 gives float locals XMM homes), and the
/// store overwrote the register under it. JVMS: `old + 2.0`.
#[test]
fn storing_a_float_local_does_not_change_its_pushed_copy() {
    let code = [
        0x22, // 0: fload_0
        0x0d, // 1: fconst_2
        0x59, // 2: dup
        0x43, // 3: fstore_0
        0x62, // 4: fadd
        0xae, // 5: freturn
    ];
    let m = compile_method(&code, 1, 1).expect("float fixture compiles");
    for x in [1.5f32, -3.25, 0.0, 1.0e6] {
        let got = call(&m, &[i64::from(x.to_bits())]);
        assert_eq!(f32::from_bits(got as u32), x + 2.0, "x = {x}");
    }
}

/// An operand live ACROSS a loop (kotlinc leaves one there for an inline
/// lambda's loop in argument position) while the loop body writes the local
/// it was loaded from. The header used to be emitted for the operand's entry
/// home — the local's own register — so every later iteration re-read the
/// updated local through it.
///
/// `f(n) = { push n; for (i = 0; i < 10; i++) n += 5; return pushed_n - n }`
/// = `-50` for every `n`.
#[test]
fn an_operand_live_across_a_loop_keeps_its_value() {
    let code = [
        0x1a, // 0: iload_0             ; the operand that outlives the loop
        0x03, // 1: iconst_0
        0x3c, // 2: istore_1            ; i = 0
        0x1b, // 3: iload_1             ; header
        0x10, 0x0a, // 4: bipush 10
        0xa2, 0x00, 0x0c, // 6: if_icmpge +12 -> 18
        0x84, 0x01, 0x01, // 9: iinc 1, 1
        0x84, 0x00, 0x05, // 12: iinc 0, 5
        0xa7, 0xff, 0xf4, // 15: goto -12 -> 3
        0x1a, // 18: iload_0
        0x64, // 19: isub
        0xac, // 20: ireturn
    ];
    let m = compile_method(&code, 1, 2).expect("loop fixture compiles");
    for n in [0i64, 7, -100, 1 << 16] {
        assert_eq!(call(&m, &[n]), -50, "n = {n}");
    }
}

/// A ROTATED loop — `goto test; body: ...; test: if<cond> body` — as ecj emits
/// every `while`/`for`. The body follows an unconditional `goto` and is named
/// only by the later bottom test, so its revival found no recorded depth and
/// refused the whole method. The operand-stack kind analysis now supplies it.
///
/// `f(n) = { s = 0; for (i = 0; i < n; i++) s += i; return s; }`
#[test]
fn a_rotated_loop_compiles_and_runs() {
    let code = [
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1            ; i = 0
        0x03, // 2: iconst_0
        0x3d, // 3: istore_2            ; s = 0
        0xa7, 0x00, 0x0a, // 4: goto +10 -> 14
        0x1c, // 7: iload_2             ; body
        0x1b, // 8: iload_1
        0x60, // 9: iadd
        0x3d, // 10: istore_2
        0x84, 0x01, 0x01, // 11: iinc 1, 1
        0x1b, // 14: iload_1            ; test
        0x1a, // 15: iload_0
        0xa1, 0xff, 0xf7, // 16: if_icmplt -9 -> 7
        0x1c, // 19: iload_2
        0xac, // 20: ireturn
    ];
    let m = compile_method(&code, 1, 3).expect(
        "a bottom-tested loop must compile: its body's entry depth comes from \
         the operand-stack kind analysis",
    );
    for n in [0i64, 1, 5, 100] {
        assert_eq!(call(&m, &[n]), n * (n - 1) / 2, "n = {n}");
    }
}
