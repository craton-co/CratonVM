// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Canonical decoded bytecode shared by verification and compiler frontends.

use crate::class_reader_error::ClassReaderError;
use crate::instruction::Instruction;
use std::collections::HashMap;
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
    // SBR round 5, lane verify: plain `Vec`s. They were `Arc<[T]>`, built with
    // `vec.into()`, which allocates a second block and copies the whole
    // instruction vector (and each target list) once per decoded method; no
    // one ever cloned the `Arc`s (the accessors hand out slices).
    instructions: Vec<VerifiedInstruction>,
    merge_targets: Vec<u32>,
    loop_headers: Vec<u32>,
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
        let (mut instructions, merge_targets, loop_headers) = decode_parts(code, true)?;
        // The instruction vector was sized for the densest possible code
        // (`len / 2 + 1`); real code decodes to roughly 40% as many entries and
        // this lives in a cache, so give the slack back (in place when the
        // allocator can).
        instructions.shrink_to_fit();
        Ok(Self {
            code: Arc::from(code),
            instructions,
            merge_targets,
            loop_headers,
        })
    }
}

/// A set of bytecode offsets below 65536 as a bitmap, inline for code up to
/// 2048 bytes.
struct StartBits {
    inline: [u64; 32],
    heap: Vec<u64>,
}

impl StartBits {
    fn new(code_len: usize) -> Self {
        let words = code_len / 64 + 1;
        Self {
            inline: [0; 32],
            heap: if words > 32 {
                vec![0u64; words]
            } else {
                Vec::new()
            },
        }
    }

    #[inline]
    fn words(&self) -> &[u64] {
        if self.heap.is_empty() {
            &self.inline
        } else {
            &self.heap
        }
    }

    #[inline]
    fn insert(&mut self, pc: usize) {
        let words: &mut [u64] = if self.heap.is_empty() {
            &mut self.inline
        } else {
            &mut self.heap
        };
        if let Some(word) = words.get_mut(pc >> 6) {
            *word |= 1u64 << (pc & 63);
        }
    }

    #[inline]
    fn contains(&self, pc: usize) -> bool {
        self.words()
            .get(pc >> 6)
            .is_some_and(|word| (word >> (pc & 63)) & 1 == 1)
    }
}

/// The decoded, validated instructions of one method and nothing else: no
/// copy of the code, no merge/loop analysis, and -- unlike [`verified_code`] --
/// not entered in the process cache.
///
/// SBR round 5, lane verify: the verifier decodes EVERY method of every class it
/// loads, once, to walk it. Going through the cache for that costs a hash of the
/// code, a shard lock, an `Arc` and a second copy of the code per method, plus
/// the later `clear()` of the shard, to prime an entry the compilers will want
/// for the few hundred methods that ever get hot. A caller that only walks the
/// instructions asks for this instead. The validation (and so every error,
/// message included) is exactly [`verified_code`]'s: both run [`decode_parts`].
#[derive(Debug)]
pub struct DecodedInstructions {
    instructions: Vec<VerifiedInstruction>,
}

impl DecodedInstructions {
    pub fn instructions(&self) -> &[VerifiedInstruction] {
        &self.instructions
    }
}

/// Decode and validate `code` without caching or analysing it further; see
/// [`DecodedInstructions`].
pub fn decode_instructions(code: &[u8]) -> Result<DecodedInstructions, ClassReaderError> {
    let (instructions, _, _) = decode_parts(code, false)?;
    Ok(DecodedInstructions { instructions })
}

/// The shared decode + validation: the instruction vector, and (when
/// `collect_targets`) the sorted merge-target and loop-header sets.
#[allow(clippy::type_complexity)]
fn decode_parts(
    code: &[u8],
    collect_targets: bool,
) -> Result<(Vec<VerifiedInstruction>, Vec<u32>, Vec<u32>), ClassReaderError> {
    {
        if code.len() > u16::MAX as usize {
            return Err(ClassReaderError::InvalidClassData {
                message: format!("method bytecode length {} exceeds 65535", code.len()),
            });
        }

        // SBR round 5, lane warmup: every method of every loaded class is
        // decoded here at verification time, so this is a startup cost. The
        // instruction vector is sized up front, branch targets are visited in
        // place (no `Vec` per branch), and the merge/loop sets are sorted
        // vectors instead of `BTreeSet`s -- the same sorted, de-duplicated
        // contents.
        let mut instructions = Vec::with_capacity(code.len() / 2 + 1);
        // The instruction starts as a bitmap (`pc` is below 65536 here), so the
        // branch-target check below is a bit test, not a binary search per
        // target (a `lookupswitch` has hundreds).
        let mut starts = StartBits::new(code.len());
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
            starts.insert(pc);
            instructions.push(VerifiedInstruction {
                pc: pc as u32,
                next_pc: next_pc as u32,
                instruction,
            });
            pc = next_pc;
        }

        let is_start = |target: usize| starts.contains(target);
        let mut merge_targets: Vec<u32> = Vec::new();
        let mut loop_headers: Vec<u32> = Vec::new();
        for decoded in &instructions {
            let source = decoded.pc as usize;
            let mut bad_target: Option<usize> = None;
            visit_instruction_targets(&decoded.instruction, source, code.len(), |target| {
                if bad_target.is_some() {
                    return;
                }
                if !is_start(target) {
                    bad_target = Some(target);
                    return;
                }
                if collect_targets {
                    merge_targets.push(target as u32);
                    if target <= source {
                        loop_headers.push(target as u32);
                    }
                }
            })?;
            if let Some(target) = bad_target {
                return Err(ClassReaderError::InvalidClassData {
                    message: format!(
                        "branch at offset {source} targets {target}, which is not an instruction boundary"
                    ),
                });
            }
            if collect_targets && is_conditional(&decoded.instruction) {
                let fallthrough = decoded.next_pc as usize;
                if fallthrough < code.len() {
                    merge_targets.push(decoded.next_pc);
                }
            }
        }
        merge_targets.sort_unstable();
        merge_targets.dedup();
        loop_headers.sort_unstable();
        loop_headers.dedup();

        Ok((instructions, merge_targets, loop_headers))
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
    // `FxHasher` (not the SipHash `DefaultHasher`): the hash is only a bucket
    // key checked against the full byte slice, and this runs for every method
    // of every loaded class. The multiply leaves the low bits weak, so the
    // shard comes from the high ones.
    let mut hasher = rustc_hash::FxHasher::default();
    code.hash(&mut hasher);
    let hash = hasher.finish();
    let shard_index = (hash >> 59) as usize & (CACHE_SHARDS - 1);

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
    let mut out = Vec::new();
    visit_instruction_targets(instruction, pc, code_len, |target| out.push(target))?;
    Ok(out)
}

/// Call `f` with each checked branch/switch target of `instruction`, in
/// operand order (default first for the switches). The first out-of-range or
/// overflowing target is the error; targets before it have already been
/// visited, which no caller observes (they all discard the partial walk on
/// `Err`).
fn visit_instruction_targets(
    instruction: &Instruction,
    pc: usize,
    code_len: usize,
    mut f: impl FnMut(usize),
) -> Result<(), ClassReaderError> {
    use Instruction::*;
    match instruction {
        Ifeq(offset) | Ifne(offset) | Iflt(offset) | Ifge(offset) | Ifgt(offset) | Ifle(offset)
        | IfIcmpeq(offset) | IfIcmpne(offset) | IfIcmplt(offset) | IfIcmpge(offset)
        | IfIcmpgt(offset) | IfIcmple(offset) | IfAcmpeq(offset) | IfAcmpne(offset)
        | Ifnull(offset) | Ifnonnull(offset) | Goto(offset) | Jsr(offset) => {
            f(checked_target(pc, *offset as i64, code_len)?);
        }
        GotoW(offset) | JsrW(offset) => f(checked_target(pc, *offset as i64, code_len)?),
        Tableswitch(ts) => {
            f(checked_target(pc, ts.default as i64, code_len)?);
            for offset in &ts.offsets {
                f(checked_target(pc, *offset as i64, code_len)?);
            }
        }
        Lookupswitch(ls) => {
            f(checked_target(pc, ls.default as i64, code_len)?);
            for (_, offset) in &ls.pairs {
                f(checked_target(pc, *offset as i64, code_len)?);
            }
        }
        _ => {}
    }
    Ok(())
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
mod tests;
