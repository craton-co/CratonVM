// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Tests of `ir::latin1_char_class` (j27 wave 4, lane LATIN1).

use super::*;
use std::collections::HashMap;


/// The Java answer over Latin-1, independent of the JDK tables: Rust's
/// Unicode `char` properties, which coincide with the Java definitions below
/// U+0100 (no `Nl`, `Lm` or `Other_Alphabetic` there, and `Nd` is ASCII
/// only), and `Character.isWhitespace`'s documented set (`SPACE_SEPARATOR`
/// but not the no-break U+00A0, plus U+0009..U+000D and U+001C..U+001F).
fn latin1_reference(class: Latin1CharClass, ch: u8) -> bool {
    let c = char::from(ch);
    match class {
        Latin1CharClass::Digit => c.is_ascii_digit(),
        Latin1CharClass::Letter => c.is_alphabetic(),
        Latin1CharClass::LetterOrDigit => c.is_alphabetic() || c.is_ascii_digit(),
        Latin1CharClass::Whitespace => matches!(ch, 0x09..=0x0D | 0x1C..=0x20),
        Latin1CharClass::UpperCase => c.is_uppercase(),
        Latin1CharClass::LowerCase => c.is_lowercase(),
    }
}

/// How many Latin-1 code points each predicate admits (JDK 25 over
/// `0..=255`: 10 digits; 52 ASCII + ª µ º + 23 + 31 + 8 letters; ...).
fn latin1_count(class: Latin1CharClass) -> usize {
    match class {
        Latin1CharClass::Digit | Latin1CharClass::Whitespace => 10,
        Latin1CharClass::Letter => 117,
        Latin1CharClass::LetterOrDigit => 127,
        Latin1CharClass::UpperCase => 56,
        Latin1CharClass::LowerCase => 61,
    }
}

/// Code points outside Latin-1 (negative, beyond U+10FFFF, and real ones)
/// with Java's answer from the Unicode database (`Character`'s rules:
/// `isUpperCase` / `isLowerCase` include `Other_Uppercase` /
/// `Other_Lowercase`, `isWhitespace` excludes the no-break spaces). Only
/// the CALL may answer them; a Latin-1 table read at `cp & 0xFF` would be
/// wrong on several (U+0660 reads `` ` ``, U+0130 reads `0`).
fn beyond_latin1(class: Latin1CharClass) -> &'static [(i32, bool)] {
    match class {
        Latin1CharClass::Digit => &[
            (0x0660, true),
            (0xFF19, true),
            (0x1D7CE, true),
            (0x2070, false),
            (0x0100, false),
            (-1, false),
            (0x11_0030, false),
        ],
        Latin1CharClass::Letter => &[
            (0x0100, true),
            (0x03B1, true),
            (0x4E00, true),
            (0x2160, false),
            (0x0300, false),
            (i32::MIN, false),
        ],
        Latin1CharClass::LetterOrDigit => &[
            (0x0660, true),
            (0x0100, true),
            (0x2160, false),
            (0x2028, false),
            (-1, false),
        ],
        Latin1CharClass::Whitespace => &[
            (0x2028, true),
            (0x2029, true),
            (0x3000, true),
            (0x1680, true),
            (0x2007, false),
            (0x202F, false),
            (0xFEFF, false),
            (0x10_0020, false),
        ],
        Latin1CharClass::UpperCase => &[
            (0x0130, true),
            (0x0178, true),
            (0x2160, true),
            (0x0131, false),
            (0x01C5, false),
            (0x1_0020, false),
        ],
        Latin1CharClass::LowerCase => &[
            (0x0131, true),
            (0x017F, true),
            (0x02B0, true),
            (0x0130, false),
            (0x01C5, false),
            (-0x100, false),
        ],
    }
}

/// The Java value of data node `id` for parameter `cp`, following the
/// control of a `Phi`'s merge to the arm that ran; an `Op::Call` answers
/// `call(argument)`.
struct Eval<'g> {
    g: &'g Graph,
    cp: i64,
    call: &'g dyn Fn(i64) -> i64,
}

impl Eval<'_> {
    fn value(&self, id: NodeId) -> i64 {
        let n = &self.g.nodes[id as usize];
        let x = |k: usize| self.value(n.inputs.as_slice()[k]);
        let raw = match n.op {
            Op::Const(c) => c,
            Op::Param(_) => self.cp,
            Op::And => x(0) & x(1),
            Op::Or => x(0) | x(1),
            Op::Neg => x(0).wrapping_neg(),
            Op::I2L | Op::L2I => x(0),
            // Casts: `lushr` / `iushr` on the bit patterns.
            Op::UShr if n.ty == IrType::Long => ((x(0) as u64) >> (x(1) & 63)) as i64,
            Op::UShr => i64::from((x(0) as u32) >> (x(1) & 31)),
            Op::Cmp(CmpOp::Eq) => i64::from(x(0) == x(1)),
            Op::Call { .. } => (self.call)(x(2)),
            Op::Phi => {
                let merge = &self.g.nodes[n.inputs.as_slice()[0] as usize];
                let arm = merge
                    .inputs
                    .as_slice()
                    .iter()
                    .position(|&c| self.live(c))
                    .expect("a live predecessor");
                x(1 + arm)
            }
            ref op => panic!("unexpected {op:?} at n{id}"),
        };
        // Cast: an `int` node's value is its low 32 bits, sign-extended.
        if n.ty == IrType::Int {
            i64::from(raw as i32)
        } else {
            raw
        }
    }

    /// Does control reach `id` for this `cp`?
    fn live(&self, id: NodeId) -> bool {
        let n = &self.g.nodes[id as usize];
        let inputs = n.inputs.as_slice();
        match n.op {
            Op::Proj(k) if self.g.nodes[inputs[0] as usize].op == Op::If => {
                let iff = self.g.nodes[inputs[0] as usize].inputs.as_slice();
                let taken = self.value(iff[1]) != 0;
                self.live(iff[0]) && taken == (k == 0)
            }
            Op::Merge => inputs.iter().any(|&c| self.live(c)),
            _ => inputs.first().map_or(true, |&c| self.live(c)),
        }
    }
}

fn count(g: &Graph, pred: impl Fn(&Op) -> bool) -> usize {
    g.nodes.iter().filter(|n| pred(&n.op)).count()
}

fn returned(g: &Graph) -> NodeId {
    let rets: Vec<_> = g.nodes.iter().filter(|n| n.op == Op::Return).collect();
    assert_eq!(rets.len(), 1, "one return");
    *rets[0].inputs.as_slice().last().expect("a returned value")
}

/// `static boolean f(int cp) { <prefix> return Character.<class>(cp'); }`,
/// the `invokestatic` right after `prefix`, with the planner's call row.
fn built(class: Latin1CharClass, descriptor: &'static str, prefix: &[u8]) -> Graph {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "java/lang/Character",
        method_name: class.method_name(),
        descriptor,
        num_jit_args: 1,
        return_type: b'Z',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut code = prefix.to_vec();
    let pc = code.len();
    code.extend_from_slice(&[0xb8, 0x00, 0x01, 0xac]);
    let len = code.len();
    code.extend_from_slice(&[0, 0]);
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Int]);
    let row = (info as *const JitInvokeInfo as usize, 1usize, b'Z');
    builder.set_invoke_info(HashMap::from([(pc, row)]));
    builder.build(&code, len).expect("IR build")
}

/// `iload_0`.
const PLAIN: [u8; 1] = [0x1a];
/// `iload_0; sipush 255; iand`: the `b & 0xFF` of a byte loop.
const MASKED: [u8; 5] = [0x1a, 0x11, 0x00, 0xFF, 0x7e];

#[test]
fn the_jdk_tables_match_independent_references_on_all_256_code_points() {
    for class in Latin1CharClass::ALL {
        for ch in 0..=255u8 {
            assert_eq!(
                class.jdk_answer(ch),
                latin1_reference(class, ch),
                "{class:?}({ch:#04x})"
            );
        }
        let admitted = (0..=255u8).filter(|&ch| class.jdk_answer(ch)).count();
        assert_eq!(admitted, latin1_count(class), "{class:?} count");
    }
}

#[test]
fn the_bitmaps_hold_exactly_the_jdk_answers() {
    for class in Latin1CharClass::ALL {
        let words = class.bitmap();
        for ch in 0..=255u8 {
            let bit = words[usize::from(ch >> 6)] >> (ch & 63) & 1 == 1;
            assert_eq!(bit, class.jdk_answer(ch), "{class:?}({ch:#04x})");
        }
    }
}

#[test]
fn the_range_test_admits_exactly_the_latin1_code_points() {
    let mut g = Graph::empty(0);
    let cp = g.add(Op::Param(0), IrType::Int, vec![], None);
    let test = in_latin1(&mut g, cp, 3);
    let no_call = |_: i64| -> i64 { panic!("no call here") };
    let extremes = [i32::MIN, i32::MIN + 0xFF, -1, 0x10FFFF, 0x110000, i32::MAX, 0x7FFF_FF00];
    for v in (-300..=600).chain(extremes) {
        let e = Eval { g: &g, cp: i64::from(v), call: &no_call };
        assert_eq!(e.value(test) == 1, (0..=255).contains(&v), "cp {v:#x}");
    }
}

#[test]
fn the_table_chain_computes_every_latin1_answer() {
    for class in Latin1CharClass::ALL {
        let mut g = Graph::empty(0);
        let cp = g.add(Op::Param(0), IrType::Int, vec![], None);
        let answer = table_answer(&mut g, class, cp, 3);
        let no_call = |_: i64| -> i64 { panic!("no call here") };
        for ch in 0..=255u8 {
            let e = Eval { g: &g, cp: i64::from(ch), call: &no_call };
            let want = i64::from(latin1_reference(class, ch));
            assert_eq!(e.value(answer), want, "{class:?}({ch:#04x})");
        }
    }
}





