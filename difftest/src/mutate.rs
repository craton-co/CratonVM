// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Bytecode mutation — design §3.1 tier 3.
//!
//! A **format-aware, in-place** mutator over a compiled `.class`: it parses the
//! constant pool and rewrites the value bytes of a numeric constant
//! (`Integer`/`Long`/`Float`/`Double`, the operands an `ldc` loads) to a
//! different "interesting" value. Because the edit is the **same byte length**,
//! every offset downstream is unchanged, so the mutant is **always
//! structurally valid and verification-clean** — the design's "re-verify each
//! mutant" step is satisfied by construction (no garbage class files), and the
//! mutation is *semantics-perturbing* (the program now loads a different
//! constant), which is exactly the differential signal we want.
//!
//! This is the slow differential tier's seed source. The fast in-process
//! libFuzzer panic-tier lives in `fuzz/fuzz_targets/difftest_bytecode.rs` and
//! reuses this same [`mutate_constant`] entry point (design §3.1 / §4 Step 5).

use crate::generate::Rng;

/// A numeric constant-pool constant we can perturb in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericKind {
    /// `CONSTANT_Integer` (tag 3), 4 value bytes.
    Integer,
    /// `CONSTANT_Float` (tag 4), 4 value bytes.
    Float,
    /// `CONSTANT_Long` (tag 5), 8 value bytes (occupies two CP slots).
    Long,
    /// `CONSTANT_Double` (tag 6), 8 value bytes (occupies two CP slots).
    Double,
}

impl NumericKind {
    fn value_len(self) -> usize {
        match self {
            NumericKind::Integer | NumericKind::Float => 4,
            NumericKind::Long | NumericKind::Double => 8,
        }
    }
}

/// A numeric constant located in the pool: its kind and the byte offset of its
/// value (just past the 1-byte tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumericConstant {
    pub kind: NumericKind,
    pub value_offset: usize,
}

fn u16_at(b: &[u8], off: usize) -> Option<usize> {
    let hi = *b.get(off)? as usize;
    let lo = *b.get(off + 1)? as usize;
    Some((hi << 8) | lo)
}

/// Walk the constant pool and return every numeric constant's location.
///
/// Returns an empty vec if the header is malformed or there are no numeric
/// constants (so callers treat "nothing to mutate" the same as a parse miss).
pub fn numeric_constants(class: &[u8]) -> Vec<NumericConstant> {
    let mut out = Vec::new();
    // magic(4) minor(2) major(2) constant_pool_count(2) then entries at 10.
    if class.len() < 10 || class[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return out;
    }
    let cp_count = match u16_at(class, 8) {
        Some(n) => n,
        None => return out,
    };
    let mut off = 10usize;
    let mut index = 1usize;
    while index < cp_count {
        let tag = match class.get(off) {
            Some(t) => *t,
            None => return out, // truncated
        };
        let value_off = off + 1;
        // Advance per JVMS 4.4 entry shapes.
        let advance = match tag {
            1 => {
                // Utf8: 2-byte length + bytes
                let len = match u16_at(class, value_off) {
                    Some(l) => l,
                    None => return out,
                };
                3 + len
            }
            3 => {
                out.push(NumericConstant {
                    kind: NumericKind::Integer,
                    value_offset: value_off,
                });
                5
            }
            4 => {
                out.push(NumericConstant {
                    kind: NumericKind::Float,
                    value_offset: value_off,
                });
                5
            }
            5 => {
                out.push(NumericConstant {
                    kind: NumericKind::Long,
                    value_offset: value_off,
                });
                index += 1; // Long occupies two slots
                9
            }
            6 => {
                out.push(NumericConstant {
                    kind: NumericKind::Double,
                    value_offset: value_off,
                });
                index += 1; // Double occupies two slots
                9
            }
            7 | 8 | 16 | 19 | 20 => 3,       // 2-byte operand
            9 | 10 | 11 | 12 | 17 | 18 => 5, // 4-byte operand
            15 => 4,                         // MethodHandle: 3-byte operand
            _ => return out,                 // unknown tag — bail conservatively
        };
        // Bound the value bytes we intend to read/write.
        if value_off + (advance - 1) > class.len() {
            return out;
        }
        off += advance;
        index += 1;
    }
    out
}

const INT_POOL: &[i32] = &[0, 1, -1, i32::MIN, i32::MAX, 2, -2, 256, -256, 65535];
const LONG_POOL: &[i64] = &[0, 1, -1, i64::MIN, i64::MAX, 2, -2, 4294967296];
/// f32 bit patterns: 0.0, -0.0, NaN, +Inf, -Inf, 1.0, -1.0.
const FLOAT_BITS_POOL: &[u32] = &[
    0x0000_0000,
    0x8000_0000,
    0x7FC0_0000,
    0x7F80_0000,
    0xFF80_0000,
    0x3F80_0000,
    0xBF80_0000,
];
/// f64 bit patterns: 0.0, -0.0, NaN, +Inf, -Inf, 1.0, -1.0.
const DOUBLE_BITS_POOL: &[u64] = &[
    0x0000_0000_0000_0000,
    0x8000_0000_0000_0000,
    0x7FF8_0000_0000_0000,
    0x7FF0_0000_0000_0000,
    0xFFF0_0000_0000_0000,
    0x3FF0_0000_0000_0000,
    0xBFF0_0000_0000_0000,
];

/// Pick a pool value that differs from the original 4/8 big-endian bytes.
fn pick_different_u32(orig: u32, pool: &[u32], rng: &mut Rng) -> u32 {
    for _ in 0..pool.len() {
        let cand = pool[rng.below(pool.len())];
        if cand != orig {
            return cand;
        }
    }
    orig ^ 0xFFFF_FFFF
}

fn pick_different_u64(orig: u64, pool: &[u64], rng: &mut Rng) -> u64 {
    for _ in 0..pool.len() {
        let cand = pool[rng.below(pool.len())];
        if cand != orig {
            return cand;
        }
    }
    orig ^ 0xFFFF_FFFF_FFFF_FFFF
}

/// Produce one mutant of `class` by rewriting a randomly-chosen numeric
/// constant's value to a different interesting value. Returns `None` when the
/// class has no numeric constants (nothing to perturb).
pub fn mutate_constant(class: &[u8], rng: &mut Rng) -> Option<Vec<u8>> {
    let consts = numeric_constants(class);
    if consts.is_empty() {
        return None;
    }
    let target = consts[rng.below(consts.len())];
    let mut out = class.to_vec();
    let off = target.value_offset;
    match target.kind {
        NumericKind::Integer => {
            let orig = u32::from_be_bytes(out[off..off + 4].try_into().ok()?);
            let new = pick_different_u32(orig, &int_pool_bits(), rng);
            out[off..off + 4].copy_from_slice(&new.to_be_bytes());
        }
        NumericKind::Float => {
            let orig = u32::from_be_bytes(out[off..off + 4].try_into().ok()?);
            let new = pick_different_u32(orig, FLOAT_BITS_POOL, rng);
            out[off..off + 4].copy_from_slice(&new.to_be_bytes());
        }
        NumericKind::Long => {
            let orig = u64::from_be_bytes(out[off..off + 8].try_into().ok()?);
            let new = pick_different_u64(orig, &long_pool_bits(), rng);
            out[off..off + 8].copy_from_slice(&new.to_be_bytes());
        }
        NumericKind::Double => {
            let orig = u64::from_be_bytes(out[off..off + 8].try_into().ok()?);
            let new = pick_different_u64(orig, DOUBLE_BITS_POOL, rng);
            out[off..off + 8].copy_from_slice(&new.to_be_bytes());
        }
    }
    let _ = target.kind.value_len(); // (documents the invariant: lengths match)
    Some(out)
}

fn int_pool_bits() -> Vec<u32> {
    INT_POOL.iter().map(|&v| v as u32).collect()
}
fn long_pool_bits() -> Vec<u64> {
    LONG_POOL.iter().map(|&v| v as u64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal hand-built class blob: header + a tiny constant pool holding
    /// one Integer(3)=7 and one Long(5)=9, used to exercise the CP walker and
    /// the in-place mutation without needing javac.
    fn fake_class() -> Vec<u8> {
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x41];
        // constant_pool_count = 4 (entries 1..3; the Long at index 2 eats slot 3)
        b.extend_from_slice(&[0x00, 0x04]);
        // #1 Integer = 7
        b.push(3);
        b.extend_from_slice(&7i32.to_be_bytes());
        // #2 Long = 9 (occupies #2 and #3)
        b.push(5);
        b.extend_from_slice(&9i64.to_be_bytes());
        b
    }

    #[test]
    fn walks_numeric_constants() {
        let c = fake_class();
        let consts = numeric_constants(&c);
        assert_eq!(consts.len(), 2);
        assert_eq!(consts[0].kind, NumericKind::Integer);
        assert_eq!(consts[1].kind, NumericKind::Long);
        // The Integer value is at offset 11 (after 10-byte header + 1 tag).
        assert_eq!(consts[0].value_offset, 11);
        assert_eq!(i32::from_be_bytes(c[11..15].try_into().unwrap()), 7);
    }

    #[test]
    fn mutation_changes_a_value_and_preserves_length() {
        let c = fake_class();
        let mut rng = Rng::new(1);
        let m = mutate_constant(&c, &mut rng).expect("has numeric constants");
        assert_eq!(m.len(), c.len(), "mutation must be length-preserving");
        assert_ne!(m, c, "mutation must change at least one byte");
        // The header (magic + version + cp_count) is untouched.
        assert_eq!(&m[0..10], &c[0..10]);
    }

    #[test]
    fn mutation_is_reproducible_for_a_seed() {
        let c = fake_class();
        let a = mutate_constant(&c, &mut Rng::new(42));
        let b = mutate_constant(&c, &mut Rng::new(42));
        assert_eq!(a, b);
    }

    #[test]
    fn no_numeric_constants_returns_none() {
        // A header with cp_count=1 (empty pool) has nothing to mutate.
        let empty = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x41, 0, 1];
        assert!(numeric_constants(&empty).is_empty());
        assert!(mutate_constant(&empty, &mut Rng::new(1)).is_none());
    }

    #[test]
    fn rejects_non_class_blob() {
        assert!(numeric_constants(b"not a class").is_empty());
    }
}
