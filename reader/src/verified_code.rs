// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Canonical decoded bytecode shared by verification and compiler frontends.

use crate::class_reader_error::ClassReaderError;
use crate::instruction::Instruction;
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex};

const CACHE_SHARDS: usize = 16;
const MAX_ENTRIES_PER_SHARD: usize = 256;
const MAX_CODE_BYTES_PER_SHARD: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedInstruction {
    pub pc: u32,
    pub next_pc: u32,
    pub instruction: Instruction,
}

/// Decode- and control-flow-validated, immutable method bytecode.
///
/// This is the common pre-IR contract. Branches are guaranteed to land on an
/// instruction boundary, and all consumers observe the same instruction
/// lengths, merge targets, and loop headers.
#[derive(Debug)]
pub struct VerifiedCode {
    code: Arc<[u8]>,
    instructions: Arc<[VerifiedInstruction]>,
    merge_targets: Arc<[u32]>,
    loop_headers: Arc<[u32]>,
}

impl VerifiedCode {
    pub fn code(&self) -> &[u8] {
        &self.code
    }

    pub fn code_arc(&self) -> Arc<[u8]> {
        Arc::clone(&self.code)
    }

    pub fn instructions(&self) -> &[VerifiedInstruction] {
        &self.instructions
    }

    pub fn merge_targets(&self) -> &[u32] {
        &self.merge_targets
    }

    pub fn loop_headers(&self) -> &[u32] {
        &self.loop_headers
    }

    pub fn instruction_at(&self, pc: usize) -> Option<&VerifiedInstruction> {
        self.instructions
            .binary_search_by_key(&(pc as u32), |instruction| instruction.pc)
            .ok()
            .map(|index| &self.instructions[index])
    }

    pub fn is_instruction_start(&self, pc: usize) -> bool {
        self.instruction_at(pc).is_some()
    }

    /// The **normal** (non-exceptional) control-flow successors of the
    /// instruction at `pc`: its branch/switch targets plus the fall-through,
    /// where the opcode has one.
    ///
    /// Deliberately excludes exception edges. A caller that walks from pc 0
    /// with only these edges therefore visits exactly the instructions the
    /// method can reach *without* throwing — i.e. everything except the bodies
    /// of its `catch`/`finally` handlers. That is the reachability the
    /// compiler frontends want, because a JIT frame never enters its own
    /// handler: an exception makes the compiled body return the `i64::MIN`
    /// sentinel and the runtime re-runs (or resumes) the method in the
    /// interpreter, which is what actually consults the exception table.
    ///
    /// Returns an empty vec for a pc that is not an instruction boundary.
    pub fn successors(&self, pc: usize) -> Vec<usize> {
        let Some(decoded) = self.instruction_at(pc) else {
            return Vec::new();
        };
        // Already validated by `decode`, which rejects any out-of-range or
        // mid-instruction target, so the error arm is unreachable here.
        let mut out =
            instruction_targets(&decoded.instruction, pc, self.code.len()).unwrap_or_default();
        if falls_through(&decoded.instruction) && (decoded.next_pc as usize) < self.code.len() {
            out.push(decoded.next_pc as usize);
        }
        out
    }

    fn decode(code: &[u8]) -> Result<Self, ClassReaderError> {
        if code.len() > u16::MAX as usize {
            return Err(ClassReaderError::InvalidClassData {
                message: format!("method bytecode length {} exceeds 65535", code.len()),
            });
        }

        let mut instructions = Vec::new();
        let mut pc = 0usize;
        while pc < code.len() {
            let (instruction, next_pc) = Instruction::decode(code, pc)?;
            if next_pc <= pc || next_pc > code.len() {
                return Err(ClassReaderError::InvalidClassData {
                    message: format!(
                        "instruction at offset {pc} advanced to invalid offset {next_pc}"
                    ),
                });
            }
            instructions.push(VerifiedInstruction {
                pc: pc as u32,
                next_pc: next_pc as u32,
                instruction,
            });
            pc = next_pc;
        }

        let is_start = |target: usize| {
            instructions
                .binary_search_by_key(&(target as u32), |instruction| instruction.pc)
                .is_ok()
        };
        let mut merge_targets = BTreeSet::new();
        let mut loop_headers = BTreeSet::new();
        for decoded in &instructions {
            let source = decoded.pc as usize;
            let targets = instruction_targets(&decoded.instruction, source, code.len())?;
            for target in targets {
                if !is_start(target) {
                    return Err(ClassReaderError::InvalidClassData {
                        message: format!(
                            "branch at offset {source} targets {target}, which is not an instruction boundary"
                        ),
                    });
                }
                merge_targets.insert(target as u32);
                if target <= source {
                    loop_headers.insert(target as u32);
                }
            }
            if is_conditional(&decoded.instruction) {
                let fallthrough = decoded.next_pc as usize;
                if fallthrough < code.len() {
                    merge_targets.insert(decoded.next_pc);
                }
            }
        }

        Ok(Self {
            code: Arc::from(code),
            instructions: instructions.into(),
            merge_targets: merge_targets.into_iter().collect::<Vec<_>>().into(),
            loop_headers: loop_headers.into_iter().collect::<Vec<_>>().into(),
        })
    }
}

#[derive(Default)]
struct CacheShard {
    by_hash: HashMap<u64, Vec<Arc<VerifiedCode>>>,
    entries: usize,
    code_bytes: usize,
}

static VERIFIED_CODE_CACHE: LazyLock<Vec<Mutex<CacheShard>>> = LazyLock::new(|| {
    (0..CACHE_SHARDS)
        .map(|_| Mutex::new(CacheShard::default()))
        .collect()
});

/// Return the process-shared canonical analysis for `code`.
///
/// The collision bucket compares full byte slices, so hashing is only an
/// accelerator and never a correctness boundary. Each shard is strictly
/// bounded; reaching either limit drops that shard's old analyses before the
/// new entry is published.
pub fn verified_code(code: &[u8]) -> Result<Arc<VerifiedCode>, ClassReaderError> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    code.hash(&mut hasher);
    let hash = hasher.finish();
    let shard_index = hash as usize & (CACHE_SHARDS - 1);

    {
        let shard = VERIFIED_CODE_CACHE[shard_index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(bucket) = shard.by_hash.get(&hash) {
            if let Some(found) = bucket.iter().find(|entry| entry.code() == code) {
                return Ok(Arc::clone(found));
            }
        }
    }

    let decoded = Arc::new(VerifiedCode::decode(code)?);
    let mut shard = VERIFIED_CODE_CACHE[shard_index]
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(bucket) = shard.by_hash.get(&hash) {
        if let Some(found) = bucket.iter().find(|entry| entry.code() == code) {
            return Ok(Arc::clone(found));
        }
    }
    if shard.entries >= MAX_ENTRIES_PER_SHARD
        || shard.code_bytes.saturating_add(code.len()) > MAX_CODE_BYTES_PER_SHARD
    {
        shard.by_hash.clear();
        shard.entries = 0;
        shard.code_bytes = 0;
    }
    shard
        .by_hash
        .entry(hash)
        .or_default()
        .push(Arc::clone(&decoded));
    shard.entries += 1;
    shard.code_bytes += code.len();
    Ok(decoded)
}

fn checked_target(source: usize, offset: i64, code_len: usize) -> Result<usize, ClassReaderError> {
    let target =
        (source as i64)
            .checked_add(offset)
            .ok_or_else(|| ClassReaderError::InvalidClassData {
                message: format!("branch target at offset {source} overflowed"),
            })?;
    if target < 0 || target >= code_len as i64 {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "branch at offset {source} has out of range target {target} \
                 for code length {code_len}"
            ),
        });
    }
    Ok(target as usize)
}

fn instruction_targets(
    instruction: &Instruction,
    pc: usize,
    code_len: usize,
) -> Result<Vec<usize>, ClassReaderError> {
    use Instruction::*;
    let offsets: Vec<i64> = match instruction {
        Ifeq(offset) | Ifne(offset) | Iflt(offset) | Ifge(offset) | Ifgt(offset) | Ifle(offset)
        | IfIcmpeq(offset) | IfIcmpne(offset) | IfIcmplt(offset) | IfIcmpge(offset)
        | IfIcmpgt(offset) | IfIcmple(offset) | IfAcmpeq(offset) | IfAcmpne(offset)
        | Ifnull(offset) | Ifnonnull(offset) | Goto(offset) | Jsr(offset) => vec![*offset as i64],
        GotoW(offset) | JsrW(offset) => vec![*offset as i64],
        Tableswitch(ts) => std::iter::once(ts.default as i64)
            .chain(ts.offsets.iter().map(|offset| *offset as i64))
            .collect(),
        Lookupswitch(ls) => std::iter::once(ls.default as i64)
            .chain(ls.pairs.iter().map(|(_, offset)| *offset as i64))
            .collect(),
        _ => Vec::new(),
    };
    offsets
        .into_iter()
        .map(|offset| checked_target(pc, offset, code_len))
        .collect()
}

/// Does control fall out of this instruction into the textually next one?
///
/// False exactly for the unconditional transfers: `goto`/`goto_w`, the two
/// switches, every `*return`, `athrow`, and `ret` (whose successor is the
/// dynamic `jsr` return address, not the next pc — modelling it as a
/// fall-through would be wrong, and conservatively dropping it only makes
/// reachability smaller, never larger).
fn falls_through(instruction: &Instruction) -> bool {
    use Instruction::*;
    !matches!(
        instruction,
        Goto(_)
            | GotoW(_)
            | Tableswitch(_)
            | Lookupswitch(_)
            | Ireturn
            | Lreturn
            | Freturn
            | Dreturn
            | Areturn
            | Return
            | Athrow
            | Ret(_)
    )
}

fn is_conditional(instruction: &Instruction) -> bool {
    use Instruction::*;
    matches!(
        instruction,
        Ifeq(_)
            | Ifne(_)
            | Iflt(_)
            | Ifge(_)
            | Ifgt(_)
            | Ifle(_)
            | IfIcmpeq(_)
            | IfIcmpne(_)
            | IfIcmplt(_)
            | IfIcmpge(_)
            | IfIcmpgt(_)
            | IfIcmple(_)
            | IfAcmpeq(_)
            | IfAcmpne(_)
            | Ifnull(_)
            | Ifnonnull(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_cfg_finds_merges_and_loop_headers() {
        // iconst_0; istore_0; iinc 0,1; iload_0; bipush 10;
        // if_icmplt -7; return
        let code = [
            0x03, 0x3b, 0x84, 0x00, 0x01, 0x1a, 0x10, 0x0a, 0xa1, 0xff, 0xfa, 0xb1,
        ];
        let verified = verified_code(&code).unwrap();
        assert_eq!(verified.loop_headers(), &[2]);
        assert_eq!(verified.merge_targets(), &[2, 11]);
        assert_eq!(verified.instructions().len(), 7);
        assert!(verified.is_instruction_start(8));
        assert!(!verified.is_instruction_start(9));
    }

    #[test]
    fn rejects_branch_into_instruction_operand() {
        // bipush 1; goto -1 (targets the goto's first operand)
        let error = verified_code(&[0x10, 0x01, 0xa7, 0xff, 0xff]).unwrap_err();
        assert!(error.to_string().contains("instruction boundary"));
    }

    #[test]
    fn cache_returns_same_analysis_arc() {
        let first = verified_code(&[0x03, 0xac]).unwrap();
        let second = verified_code(&[0x03, 0xac]).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn rejects_truncated_instruction() {
        assert!(verified_code(&[0x11, 0x01]).is_err());
    }
}
