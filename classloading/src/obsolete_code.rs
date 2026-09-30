// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Constant-pool continuity for code a JVMTI redefinition leaves running.
//!
//! JVMTI `RedefineClasses` (JEP 109): "If a redefined method has active stack
//! frames, those active frames continue to run the bytecodes of the original
//! method." Those bytecodes name their constants by index into the ORIGINAL
//! pool, and `ClassManager::redefine_class` replaces the pool in place. HotSpot
//! keeps the old indices meaningful by merging the pools
//! (`VM_RedefineClasses::merge_cp_and_rewrite`) and keeping each obsolete
//! method with its own constant pool.
//!
//! Here the class keeps the NEW pool at the NEW indices -- so the new
//! bytecode, its verifier maps and everything compiled from it are untouched
//! -- and every constant of the old pool is appended to it unless an equal
//! constant is already there ([`merge_for_obsolete_code`]). The translation
//! that comes back maps each old index to the merged index naming the same
//! constant; the VM rewrites the operands of a running old body through it
//! (`vm::runtime::interpreter::obsolete_frames`), after which every resolver
//! and every `(class, cp index)` site cache reads the right constant with no
//! change on their side. A class redefined several times keeps one
//! translation per redefinition ([`RedefinitionHistory`]); a frame older than
//! several of them is translated through each in turn.
//!
//! Interpreter round i1 wave 19, lane L3
//! (`docs/internal/fixed-bugs/interpreter-L3-obsolete-methods-keep-no-constant-pool-FIXED-20260925.md`).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use cratonvm_reader::attribute::{
    Attribute, BootstrapMethod, ExceptionTableEntry, LineNumberEntry,
};
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::instruction::Instruction;
use cratonvm_reader::method::ClassFileMethod;
use rustc_hash::{FxHashMap, FxHashSet};

/// Redefinitions of one class whose translations are always kept. A frame
/// built before the oldest kept one is not translated (it runs as it did
/// before wave 19: its old indices read the current pool). Only redefinitions
/// that moved a constant or changed a line table count (see
/// `RedefinitionStep`). More are kept while they fit [`HISTORY_BYTE_BUDGET`].
pub const MAX_KEPT_REDEFINITIONS: usize = 8;

/// Beyond [`MAX_KEPT_REDEFINITIONS`], a class's history keeps older steps
/// while its distinct translations and line tables take at most this many
/// bytes, and it has at most [`MAX_HISTORY_STEPS`] steps (interpreter round
/// i1 wave 22, lane L3). A step equal to one just before it shares its
/// tables (`RedefinitionHistory::record`), so an agent that toggles a class
/// between two versions pays two translations for any number of toggles, and
/// a thread parked across hundreds of them still has its frames translated.
pub const HISTORY_BYTE_BUDGET: usize = 32 * 1024;

/// The most steps a class's history keeps, whatever they cost, unless the
/// stale-frame census says a frame still needs them
/// ([`RedefinitionHistory::prune`]).
pub const MAX_HISTORY_STEPS: usize = 256;

/// What a history may grow to past [`HISTORY_BYTE_BUDGET`] /
/// [`MAX_HISTORY_STEPS`] while the stale-frame census reports a frame that
/// still needs its oldest step (interpreter round i1 wave 23, lane L3): a
/// thread parked across hundreds of DIFFERENT renumbering redefinitions of
/// its class then still wakes translatable. Past these the oldest step is
/// dropped whatever the census says, so an agent retransforming a class with
/// a fresh pool every few milliseconds while some thread stays parked keeps
/// at most this much per class.
///
/// Counts each kept step's own record ([`HARD_STEP_RECORD_BYTES`]) besides
/// its tables, since wave 25: a step's translation is a few runs (wave 24),
/// so the record is most of what a step costs. Raised from 128 KiB / 1,024
/// steps in interpreter round i1 wave 25, lane L3: `retransformClasses`
/// redefines each class TWICE here (`vm::runtime::instrument`,
/// `native_retransform_classes0`, first back to the retransformation base,
/// then to the transformers' output), and each of the two moves the renamed
/// constants, so 600 retransforms of a class a thread stays parked in are
/// 1,199 steps -- the old cap dropped the parked frame's first 175 and it woke
/// reading the new pool at its old index
/// (`tools/probes/interp/L3/L3W24ParkedAcrossManyRenames.java`, `parkQxC`).
/// Since wave 26 a retransform is ONE redefinition (the swap back to the base
/// is gone), so these caps now cover about twice as many retransforms: 600 of
/// them are 600 steps. Deliberately not lowered back.
pub const HISTORY_HARD_BYTE_BUDGET: usize = 16 * HISTORY_BYTE_BUDGET;

/// See [`HISTORY_HARD_BYTE_BUDGET`].
pub const MAX_HISTORY_STEPS_HARD: usize = 4096;

/// What one kept step costs besides its tables, as the hard budget counts it:
/// the step's record, and the header of its translation's allocation.
const HARD_STEP_RECORD_BYTES: usize = std::mem::size_of::<RedefinitionStep>()
    + std::mem::size_of::<Translation>()
    + 2 * std::mem::size_of::<usize>();

/// How many of the latest steps a new one looks through for equal tables to
/// share: an A/B toggle repeats the step two back.
const SHARE_WINDOW: usize = 4;

/// The largest `constant_pool_count` a class file can carry (JVMS 4.1, a
/// `u2`): valid indices are `1..count`.
const MAX_POOL_COUNT: usize = u16::MAX as usize;

/// Deepest chain of constant-pool references a content key follows. Only a
/// malformed pool (a `CONSTANT_Dynamic` whose bootstrap arguments reach
/// itself) gets near it; such an entry is appended without deduplication.
const MAX_KEY_DEPTH: u32 = 32;

/// The class's new constant pool with the old pool's constants appended, and
/// the translation from old indices.
#[derive(Debug)]
pub struct MergedPool {
    /// The new pool, index for index, followed by every old constant the new
    /// pool has no equal of.
    pub constant_pool: ConstantPool,
    /// The new `BootstrapMethods`, index for index, followed by the old ones
    /// an appended `CONSTANT_Dynamic` / `CONSTANT_InvokeDynamic` names.
    pub bootstrap_methods: Vec<BootstrapMethod>,
    /// `translation[old index]` is the merged index of the same constant; `0`
    /// for an old slot that names nothing (index 0, the second slot of a
    /// `long` / `double`).
    pub translation: Vec<u16>,
}

/// Append `old_pool`'s constants to `new_pool` (see the module doc).
///
/// `ldc_first` lists old indices a one-byte `ldc` operand names; they are
/// placed first, so that where the new pool is short enough they land at an
/// index an `ldc` can still encode. `None` when the merged pool would not fit
/// a class file's `u2` count (or an old entry is malformed): the caller then
/// installs the new pool alone, as before wave 19.
pub fn merge_for_obsolete_code(
    old_pool: &ConstantPool,
    old_bsms: &[BootstrapMethod],
    new_pool: &ConstantPool,
    new_bsms: &[BootstrapMethod],
    ldc_first: &[u16],
) -> Option<MergedPool> {
    merge_keeping(old_pool, old_bsms, new_pool, new_bsms, ldc_first, None)
}

/// [`merge_for_obsolete_code`] carrying over only the old constants `keep`
/// marks (every one for `None`), with the components they name. An old index
/// left out translates to `0`, "names nothing".
fn merge_keeping(
    old_pool: &ConstantPool,
    old_bsms: &[BootstrapMethod],
    new_pool: &ConstantPool,
    new_bsms: &[BootstrapMethod],
    ldc_first: &[u16],
    keep: Option<&[bool]>,
) -> Option<MergedPool> {
    let mut merger = Merger::new(old_pool, old_bsms, new_pool, new_bsms);
    let mut firsts = ldc_first.to_vec();
    firsts.sort_unstable();
    firsts.dedup();
    for index in firsts {
        merger.intern_old(index, true);
    }
    for index in 1..old_pool.len() {
        if keep.is_some_and(|keep| !keep.get(index).copied().unwrap_or(false)) {
            continue;
        }
        let index = u16::try_from(index).ok()?;
        merger.intern_old(index, false);
    }
    merger.finish()
}

/// The merged pool's tail (constants appended after the new pool) below
/// which [`merge_for_obsolete_code_compacting`] does not look for dead
/// constants.
const COMPACT_TAIL_MIN: usize = 512;

/// [`merge_for_obsolete_code`] that leaves out the old constants no frame the
/// class's kept history can translate may name
/// ([`RedefinitionHistory::live_indices`]; the i22-L3 census proposal's stage
/// 3, interpreter round i1 wave 24, lane L3).
///
/// Every old constant used to be carried into every later merge, so a class
/// an agent retransformed with a fresh constant each time grew its pool by
/// that constant per retransform -- and its translations, sized by the old
/// pool, with it -- until the merge no longer fit a `u2` count, the history
/// was cleared and every older frame became untranslatable at once. Now,
/// once the plain merge's appended tail reaches [`COMPACT_TAIL_MIN`] (or the
/// plain merge does not fit), the dead constants are left out when that at
/// least halves the tail. Leaving them out renumbers the rest of the tail, so
/// the redefinition counts as one that moved constants (a step and a
/// handshake pause) -- hence the threshold, which makes that rare: at most
/// one compaction per doubling of the tail. A look that found too few dead
/// constants is not repeated until a step was dropped or the tail doubled
/// (`RedefinitionHistory::compaction_worth_a_look`, wave 25).
pub fn merge_for_obsolete_code_compacting(
    old_pool: &ConstantPool,
    old_bsms: &[BootstrapMethod],
    new_pool: &ConstantPool,
    new_bsms: &[BootstrapMethod],
    ldc_first: &[u16],
    history: Option<&RedefinitionHistory>,
) -> Option<MergedPool> {
    let plain = merge_for_obsolete_code(old_pool, old_bsms, new_pool, new_bsms, ldc_first);
    let plain_tail = plain
        .as_ref()
        .map(|merged| merged.constant_pool.len().saturating_sub(new_pool.len()));
    if plain_tail.is_some_and(|tail| tail < COMPACT_TAIL_MIN) {
        return plain;
    }
    let Some(history) = history else {
        return plain;
    };
    // A merge that fits looks again only when something could have died since
    // its last fruitless look (interpreter round i1 wave 25, lane L3).
    if plain_tail.is_some_and(|tail| !history.compaction_worth_a_look(tail)) {
        return plain;
    }
    let Some(live) = history.live_indices(old_pool.len()) else {
        return plain;
    };
    // Not worth a second merge unless dead constants could halve the tail.
    let dead = live.iter().skip(1).filter(|&&is_live| !is_live).count();
    if let Some(tail) = plain_tail.filter(|&tail| dead.saturating_mul(2) < tail) {
        history.note_compaction_look(tail, false);
        return plain;
    }
    let compacted = merge_keeping(
        old_pool,
        old_bsms,
        new_pool,
        new_bsms,
        ldc_first,
        Some(&live),
    );
    match (plain, compacted) {
        (Some(plain), Some(compacted)) => {
            let tail = compacted.constant_pool.len().saturating_sub(new_pool.len());
            let plain_tail = plain_tail.unwrap_or(0);
            let worth = tail.saturating_mul(2) <= plain_tail;
            history.note_compaction_look(plain_tail, worth);
            if worth {
                Some(compacted)
            } else {
                Some(plain)
            }
        }
        (None, compacted) => compacted,
        (plain, None) => plain,
    }
}

/// The constant-pool indices the one-byte `ldc` instructions of `methods`
/// name (decoded `Code` attributes only).
pub fn ldc_operands_of(methods: &[ClassFileMethod]) -> Vec<u16> {
    let mut out = Vec::new();
    for method in methods {
        let Some(code) = method.code() else { continue };
        let code: &[u8] = &code.code;
        let mut pc = 0usize;
        while pc < code.len() {
            let Ok((insn, next)) = Instruction::decode(code, pc) else {
                break;
            };
            if next <= pc {
                break;
            }
            if let Instruction::Ldc(index) = insn {
                out.push(u16::from(index));
            }
            pc = next;
        }
    }
    out
}

/// `code` (unpadded) with every constant-pool operand mapped through `map`.
/// The instruction layout is unchanged, so every pc, branch offset, switch
/// table and exception range stays valid. `None` when an operand does not
/// map, or a one-byte `ldc` operand maps above 255 (see
/// [`translate_code_widening`], which reports those instead).
pub fn translate_code(code: &[u8], map: &dyn Fn(u16) -> Option<u16>) -> Option<Vec<u8>> {
    match translate_code_widening(code, map, &[])? {
        (out, widened) if widened.is_empty() => Some(out),
        _ => None,
    }
}

/// [`translate_code`] that does not give up on a one-byte `ldc` whose
/// constant maps above 255: its operand byte is left as it was and the site
/// is reported as `(pc, merged index)`, for the caller to run the body on a
/// decoded stream that reads the index from there
/// (`cratonvm_reader::QuickenedCode::with_widened_ldc`; interpreter round i1
/// wave 21, lane L4).
///
/// `prior` lists the sites of `code` a previous translation already reported
/// that way: the index such an `ldc` names is the reported one, not its
/// (stale) operand byte.
pub fn translate_code_widening(
    code: &[u8],
    map: &dyn Fn(u16) -> Option<u16>,
    prior: &[(usize, u16)],
) -> Option<(Vec<u8>, Vec<(usize, u16)>)> {
    let mut out = code.to_vec();
    let mut widened = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (_, next) = Instruction::decode(code, pc).ok()?;
        if next <= pc || next > code.len() {
            return None;
        }
        match code[pc] {
            // ldc
            0x12 => {
                let old = match prior.iter().find(|&&(at, _)| at == pc) {
                    Some(&(_, index)) => index,
                    None => u16::from(*code.get(pc + 1)?),
                };
                let merged = map(old)?;
                match u8::try_from(merged) {
                    Ok(byte) => *out.get_mut(pc + 1)? = byte,
                    Err(_) => widened.push((pc, merged)),
                }
            }
            // ldc_w, ldc2_w; get/put static/field, the five invokes; new,
            // anewarray, checkcast, instanceof, multianewarray.
            0x13 | 0x14 | 0xb2..=0xba | 0xbb | 0xbd | 0xc0 | 0xc1 | 0xc5 => {
                let old = u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
                let merged = map(old)?.to_be_bytes();
                out.get_mut(pc + 1..pc + 3)?.copy_from_slice(&merged);
            }
            _ => {}
        }
        pc = next;
    }
    Some((out, widened))
}

/// `table` with every non-zero `catch_type` mapped through `map`.
pub fn translate_exception_table(
    table: &[ExceptionTableEntry],
    map: &dyn Fn(u16) -> Option<u16>,
) -> Option<Vec<ExceptionTableEntry>> {
    table
        .iter()
        .map(|entry| {
            let catch_type = match entry.catch_type {
                0 => 0,
                index => map(index)?,
            };
            Some(ExceptionTableEntry {
                catch_type,
                ..entry.clone()
            })
        })
        .collect()
}

/// One redefinition of a class, as the frames it left running see it.
///
/// A redefinition that kept every old index naming its constant AND changed
/// no method's line table leaves nothing a running old body needs, and
/// records no step at all (interpreter round i1 wave 21, lane L4): an
/// identical retransform -- a transformer that returned its input -- used to
/// take one of the [`MAX_KEPT_REDEFINITIONS`] slots all the same.
#[derive(Debug)]
struct RedefinitionStep {
    /// `class_redefinition_count()` when the redefinition had completed. A
    /// frame stamped below it was built before the swap and runs the code of
    /// the pool this step translates from.
    retired_at: u64,
    /// Old (pre-step) index -> merged (post-step) index; `0` = none. `None`:
    /// every old index kept its constant (the identity).
    translation: Option<Arc<Translation>>,
    /// The line tables of the methods whose `LineNumberTable` this step
    /// changed, as they were before it.
    replaced_lines: Box<[ReplacedLineTable]>,
    /// The fresh code of the versions whose pool this step translates from
    /// names indices below this (the longest of their own pools, which the
    /// merge placed index for index); `None` when not known. See
    /// [`RedefinitionHistory::live_indices`] (interpreter round i1 wave 24,
    /// lane L3).
    source_new_len: Option<usize>,
    /// Indices of the pool this step translates from that frames moved onto
    /// a translated copy while it was current name (past `source_new_len`):
    /// such a frame is stamped at that version, so its code needs this step
    /// ([`RedefinitionHistory::note_moved_names`]).
    source_moved: Box<[u16]>,
}

/// One run of a [`Translation`]: old indices `from..from + count` map to
/// `to..to + count`, or name nothing when `to` is `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranslationRun {
    from: u16,
    to: u16,
    count: u16,
}

/// A step's old-index -> merged-index map, kept as runs (interpreter round
/// i1 wave 24, lane L3). A merge keeps the new pool index for index and
/// appends the old constants it lacks in order, so an old pool maps onto the
/// merged one in a few long runs -- the unchanged prefix, each moved
/// constant, the re-appended tail shifted as a block -- where the dense
/// `Vec<u16>` it is built from takes two bytes per old slot. The history's
/// size then follows how much a redefinition changed, not how large the
/// merged pool has grown: a thread parked across hundreds of renaming
/// retransforms of a class used to push the history past its hard byte cap
/// (`docs/internal/fixed-bugs/interpreter-L3-untranslatable-obsolete-frames-keep-reading-the-new-pool-RETIRED-20260930.md`).
#[derive(Debug, PartialEq, Eq)]
struct Translation {
    /// The old pool's slot count: an index at or above it maps to nothing.
    len: usize,
    /// Sorted by `from`, covering `0..len` without a gap.
    runs: Box<[TranslationRun]>,
}

impl Translation {
    /// The runs of `dense` (`dense[old] = merged`, `0` = none).
    fn from_dense(dense: &[u16]) -> Self {
        let mut runs: Vec<TranslationRun> = Vec::new();
        for (index, &to) in dense.iter().enumerate() {
            // `dense` comes from a merge, whose old pool fits a `u2` count.
            let Ok(from) = u16::try_from(index) else {
                break;
            };
            if let Some(last) = runs.last_mut() {
                let continues = if to == 0 {
                    last.to == 0
                } else {
                    last.to != 0 && last.to.checked_add(last.count) == Some(to)
                };
                if continues && last.count < u16::MAX {
                    last.count += 1;
                    continue;
                }
            }
            runs.push(TranslationRun { from, to, count: 1 });
        }
        Self {
            len: dense.len(),
            runs: runs.into_boxed_slice(),
        }
    }

    /// The merged index of old `index`: `Some(0)` when it names nothing,
    /// `None` past the old pool.
    fn get(&self, index: u16) -> Option<u16> {
        if usize::from(index) >= self.len {
            return None;
        }
        let after = self.runs.partition_point(|run| run.from <= index);
        let run = self.runs.get(after.checked_sub(1)?)?;
        if run.to == 0 {
            return Some(0);
        }
        run.to.checked_add(index - run.from)
    }

    /// The bytes its runs take.
    fn byte_size(&self) -> usize {
        self.runs.len() * std::mem::size_of::<TranslationRun>()
    }

    /// Mark in `out` the merged index of every old index `keep` marks (every
    /// old index for `None`) that names something, growing `out` as needed.
    fn mark_image(&self, keep: Option<&[bool]>, out: &mut Vec<bool>) {
        for run in self.runs.iter() {
            if run.to == 0 {
                continue;
            }
            for offset in 0..run.count {
                let from = usize::from(run.from) + usize::from(offset);
                if from == 0 || !keep.map_or(true, |keep| keep.get(from).copied().unwrap_or(false)) {
                    continue;
                }
                let to = usize::from(run.to) + usize::from(offset);
                if out.len() <= to {
                    out.resize(to + 1, false);
                }
                if let Some(cell) = out.get_mut(to) {
                    *cell = true;
                }
            }
        }
    }
}

/// A method's `LineNumberTable` as a redefinition found it, kept for the
/// frames still running that body (JEP 109 obsolete methods report their own
/// lines, as HotSpot's obsolete `Method*` does).
#[derive(Debug, Clone)]
pub struct ReplacedLineTable {
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    /// Every `LineNumberTable` entry of the body, sorted by `start_pc`
    /// (stable, so equal starts keep their attribute order).
    pub lines: Arc<[LineNumberEntry]>,
}

/// The effective `LineNumberTable` of `method` (JVMS 4.7.12: the union of
/// every such attribute of its `Code`), sorted by `start_pc`; `None` for a
/// method without code.
fn method_line_table(method: &ClassFileMethod) -> Option<Vec<LineNumberEntry>> {
    let code = method.code()?;
    let mut lines = Vec::new();
    for attr in &code.attributes {
        if let Attribute::LineNumberTable(entries) = attr {
            lines.extend_from_slice(entries);
        }
    }
    lines.sort_by_key(|e| e.start_pc);
    Some(lines)
}

/// The line tables of `old` methods that `new` (the same class redefined)
/// gives a different table, or none: what [`RedefinitionHistory::record`]
/// keeps for the frames still running an old body.
pub fn replaced_line_tables(
    old: &[ClassFileMethod],
    new: &[ClassFileMethod],
) -> Vec<ReplacedLineTable> {
    let key = |e: &LineNumberEntry| (e.start_pc, e.line_number);
    let mut out = Vec::new();
    for method in old {
        let Some(before) = method_line_table(method) else {
            continue;
        };
        let after = new
            .iter()
            .find(|m| m.name == method.name && m.descriptor == method.descriptor)
            .and_then(method_line_table);
        let same = after
            .as_ref()
            .is_some_and(|after| after.iter().map(key).eq(before.iter().map(key)));
        if !same {
            out.push(ReplacedLineTable {
                name: Arc::clone(&method.name),
                descriptor: Arc::clone(&method.descriptor),
                lines: Arc::from(before),
            });
        }
    }
    out
}

/// The line of `bci` in `lines` (sorted by `start_pc`): the entry with the
/// largest `start_pc` at or below it, the last of equals -- the rule
/// `vm::runtime::stackwalker` applies to a class's current methods.
pub fn line_number_at(lines: &[LineNumberEntry], bci: usize) -> Option<u16> {
    let bci = u16::try_from(bci).unwrap_or(u16::MAX);
    let after = lines.partition_point(|e| e.start_pc <= bci);
    after
        .checked_sub(1)
        .and_then(|i| lines.get(i))
        .map(|e| e.line_number)
}

/// Mark indices `1..len` of `set`, growing it as needed.
fn mark_prefix(set: &mut Vec<bool>, len: usize) {
    if set.len() < len {
        set.resize(len, false);
    }
    for cell in set.iter_mut().take(len).skip(1) {
        *cell = true;
    }
}

/// Mark each of `indices` in `set`, growing it as needed.
fn mark_each(set: &mut Vec<bool>, indices: &[u16]) {
    for &index in indices {
        let at = usize::from(index);
        if set.len() <= at {
            set.resize(at + 1, false);
        }
        if let Some(cell) = set.get_mut(at) {
            *cell = true;
        }
    }
}

/// The per-class record of the redefinitions whose old code may still be
/// running, oldest first (`ClassManager::redefinition_history`).
///
/// A frame records `class_redefinition_count()` when it is built (its
/// stamp). Its code indexes the pool that was current then; every step
/// retired after the stamp has to be applied to reach the class's current
/// pool.
#[derive(Debug, Default)]
pub struct RedefinitionHistory {
    steps: Vec<RedefinitionStep>,
    /// A frame stamped below this cannot be translated: a step it needs was
    /// dropped ([`MAX_KEPT_REDEFINITIONS`]) or never recorded (a merge that
    /// did not fit).
    convertible_from: u64,
    /// `retired_at` of the class's last redefinition.
    latest: u64,
    /// Did the last redefinition give some old index a different constant,
    /// with a translation to repair it? `false` when every old index still
    /// names an equal constant (an unchanged pool, or one a transformer only
    /// appended to, as ASM-based agents such as ByteBuddy's inline mock maker
    /// do), and when no translation was recorded.
    last_moved_constants: bool,
    /// `class_redefinition_count()` just after the class's last
    /// redefinition began (its first advance, before the pool was replaced;
    /// see [`Self::latest_redefinition_began`]).
    latest_began: u64,
    /// The last stale-frame census ([`Self::prune`]): no frame the census
    /// can see is stamped below it. `None` until a census ran; the history
    /// is then bounded by the soft caps alone, as before wave 23.
    census_floor: Option<u64>,
    /// The longest pool (slot count) a redefinition since the last recorded
    /// step installed index for index: the fresh code of those versions --
    /// the class's current methods among them -- names indices below it.
    /// `None` when not known (no redefinition reported one). The next step
    /// keeps it as its `source_new_len` (interpreter round i1 wave 24, lane
    /// L3; [`Self::live_indices`]).
    pool_len_since_step: Option<usize>,
    /// What [`Self::note_moved_names`] recorded since the last step; the next
    /// step keeps it as its `source_moved`.
    moved_since_step: std::sync::Mutex<Vec<u16>>,
    /// Steps dropped so far, by any rule. A dropped step is the only thing
    /// that turns live constants dead ([`Self::live_indices`]).
    dropped_steps: u64,
    /// `(plain tail, dropped_steps)` of the last merge that looked for dead
    /// constants and found too few to compact
    /// (`merge_for_obsolete_code_compacting`); `(0, _)` when none did since
    /// the last compaction. Interpreter round i1 wave 25, lane L3: the look
    /// walks every kept step over the whole pool, and a thread parked across
    /// hundreds of renames keeps every constant live, so it used to cost that
    /// walk at every redefinition past the threshold for nothing.
    fruitless_compaction: std::sync::Mutex<(usize, u64)>,
}

impl RedefinitionHistory {
    /// Record a redefinition that completed at `retired_at`, with the
    /// translation its merge produced (`None`: the pool was replaced without
    /// one, so no older frame can be translated past it) and the line tables
    /// it replaced ([`replaced_line_tables`]).
    ///
    /// A translation or line table equal to one of the last few steps' is
    /// shared with it rather than kept twice, and old steps are dropped only
    /// past [`MAX_KEPT_REDEFINITIONS`] and then only while the history is
    /// over [`HISTORY_BYTE_BUDGET`] or [`MAX_HISTORY_STEPS`] (interpreter
    /// round i1 wave 22, lane L3). It used to drop the ninth-oldest step
    /// whatever it cost, so a thread parked across nine toggles of its
    /// class between two versions woke with its frames untranslatable.
    pub(crate) fn record(
        &mut self,
        retired_at: u64,
        translation: Option<Vec<u16>>,
        replaced_lines: Vec<ReplacedLineTable>,
    ) {
        self.record_window(retired_at, retired_at, translation, replaced_lines, None);
    }

    /// [`Self::record`] for a redefinition that began (advanced
    /// `class_redefinition_count()` for the first time, before replacing the
    /// pool) at `began_at` and completed at `retired_at`. A frame stamped in
    /// `began_at..retired_at` was built while the swap was under way, from
    /// either body ([`Self::latest_redefinition_began`]). Interpreter round
    /// i1 wave 23, lane L3.
    ///
    /// `new_pool_len`: the slot count of the pool the redefinition's class
    /// file brought, which the merge placed index for index (`None`: not
    /// known, and the class's merged pool is then never compacted --
    /// [`Self::live_indices`]; wave 24).
    pub(crate) fn record_window(
        &mut self,
        began_at: u64,
        retired_at: u64,
        translation: Option<Vec<u16>>,
        replaced_lines: Vec<ReplacedLineTable>,
        new_pool_len: Option<usize>,
    ) {
        if self.latest == 0 && self.pool_len_since_step.is_none() {
            // The class's first redefinition: the pool it replaces is the one
            // the class was defined with, every slot of which its own code
            // may name.
            self.pool_len_since_step = translation.as_ref().map(Vec::len);
        }
        let moved_names = self.take_moved_names();
        self.latest = retired_at;
        self.latest_began = began_at.min(retired_at);
        self.last_moved_constants = translation.as_ref().is_some_and(|translation| {
            translation
                .iter()
                .enumerate()
                .any(|(index, &merged)| merged != 0 && usize::from(merged) != index)
        });
        match translation {
            Some(translation) => {
                if !self.last_moved_constants && replaced_lines.is_empty() {
                    // Nothing an old body needs: see `RedefinitionStep`. Every
                    // old index kept its slot, so this version's pool joins
                    // the ones the next step translates from.
                    self.pool_len_since_step = self
                        .pool_len_since_step
                        .zip(new_pool_len)
                        .map(|(since, now)| since.max(now));
                    self.note_moved_names(&moved_names);
                    return;
                }
                let translation = if self.last_moved_constants {
                    Some(self.shared_translation(&translation))
                } else {
                    None
                };
                let replaced_lines = self.shared_line_tables(replaced_lines);
                self.steps.push(RedefinitionStep {
                    retired_at,
                    translation,
                    replaced_lines: replaced_lines.into_boxed_slice(),
                    source_new_len: self.pool_len_since_step,
                    source_moved: moved_names.into_boxed_slice(),
                });
                self.pool_len_since_step = new_pool_len;
                self.trim();
            }
            None => {
                self.dropped_steps = self
                    .dropped_steps
                    .wrapping_add(u64::try_from(self.steps.len()).unwrap_or(u64::MAX));
                self.steps.clear();
                self.convertible_from = retired_at;
                self.pool_len_since_step = new_pool_len;
            }
        }
    }

    /// Record that frames were just moved onto code naming `names`, indices
    /// of the class's current pool (`vm::runtime::interpreter::obsolete_frames`,
    /// when it translates a frame; interpreter round i1 wave 24, lane L3).
    /// Such a frame is stamped at the current version from then on, so these
    /// indices stay live for as long as a step after it is kept
    /// ([`Self::live_indices`]). Only indices past the current version's own
    /// pool are kept: every one below it is live anyway. Takes `&self`: the
    /// VM moves frames under the class-manager READ lock.
    pub fn note_moved_names(&self, names: &[u16]) {
        let own = self.pool_len_since_step.unwrap_or(0);
        let mut kept = self
            .moved_since_step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for &name in names {
            if usize::from(name) >= own && !kept.contains(&name) {
                kept.push(name);
            }
        }
    }

    /// The names [`Self::note_moved_names`] collected since the last
    /// redefinition, emptied.
    fn take_moved_names(&mut self) -> Vec<u16> {
        std::mem::take(
            self.moved_since_step
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// The indices of the class's current (merged) pool, `pool_len` slots,
    /// that a frame the kept history can still translate may name, or `None`
    /// when that is not known (then every index may be): the fresh code of
    /// each version since the oldest kept step names the indices its own
    /// pool placed index for index, and a frame moved onto a copy names only
    /// indices some kept step's translation produced. A constant appended for
    /// an older version that no kept step reaches is dead: the next merge
    /// need not carry it (`merge_for_obsolete_code_compacting`; the i22-L3
    /// census proposal's stage 3, interpreter round i1 wave 24, lane L3).
    ///
    /// A frame stamped at some version runs either that version's own code
    /// (indices below its `source_new_len`) or a copy it was moved onto
    /// while that version was current (the `source_moved` names); a frame
    /// stamped below the oldest kept step cannot be translated at all, so
    /// what the versions before it named does not matter. Each kept step
    /// carries what its source version names into the next one.
    pub fn live_indices(&self, pool_len: usize) -> Option<Vec<bool>> {
        let current = self.pool_len_since_step?;
        // Over the pool the next step translates from.
        let mut live: Vec<bool> = Vec::new();
        for step in &self.steps {
            mark_prefix(&mut live, step.source_new_len?);
            mark_each(&mut live, &step.source_moved);
            if let Some(translation) = step.translation.as_deref() {
                let mut image = Vec::new();
                translation.mark_image(Some(&live), &mut image);
                live = image;
            }
        }
        mark_prefix(&mut live, current);
        mark_each(
            &mut live,
            &self
                .moved_since_step
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        live.resize(pool_len, false);
        Some(live)
    }

    /// The stale-frame census (interpreter round i1 wave 23, lane L3;
    /// `vm::runtime::interpreter::obsolete_frames::prune_histories_by_census`): no
    /// frame the census can see is stamped below `floor`, so a step retired
    /// at or below it translates for nobody. Drops such steps beyond the
    /// last [`MAX_KEPT_REDEFINITIONS`] (kept whatever the census says: a
    /// compiled activation's constant-pool stamp and a frame of a thread the
    /// census cannot see are not in it), and from now on lets a step the
    /// census says is still needed outlive the soft caps, up to
    /// [`HISTORY_HARD_BYTE_BUDGET`] / [`MAX_HISTORY_STEPS_HARD`].
    ///
    /// Constant time unless it drops something: the history's growth is
    /// trimmed at [`Self::record`], and every cap a census allows is at least
    /// as loose as the one applied there, so a new floor can only release the
    /// oldest steps (the VM prunes every redefined class's history at each
    /// redefinition).
    pub fn prune(&mut self, floor: u64) {
        self.census_floor = Some(floor);
        while self.steps.len() > MAX_KEPT_REDEFINITIONS && self.census_may_drop_oldest(floor) {
            let dropped = self.steps.remove(0);
            self.convertible_from = dropped.retired_at;
            self.dropped_steps = self.dropped_steps.wrapping_add(1);
        }
    }

    /// May a census with `floor` drop the oldest step? Only when no frame it
    /// can see is stamped before the step retired, and -- when the step
    /// replaced line tables -- only past the soft caps: a Throwable's stored
    /// backtrace is not a frame, and reads the line table of the version it
    /// was captured in from here (`line_table_at`; the VM's
    /// `obsolete_frames::resolve_lines_as_captured`). Without this, the
    /// per-class census (interpreter round i1 wave 24, lane L3), which drops
    /// the steps of a class no frame runs at once, would give a backtrace
    /// captured nine redefinitions ago the CURRENT body's line where wave 23
    /// kept its own (HotSpot answers -1 once no frame runs the version; the
    /// census proposal's stage 4).
    fn census_may_drop_oldest(&self, floor: u64) -> bool {
        let Some(oldest) = self.steps.first() else {
            return false;
        };
        oldest.retired_at <= floor
            && (oldest.replaced_lines.is_empty()
                || self.steps.len() > MAX_HISTORY_STEPS
                || self.distinct_bytes() > HISTORY_BYTE_BUDGET)
    }

    /// Drop the oldest steps the rules of [`Self::record`] and
    /// [`Self::prune`] no longer keep.
    fn trim(&mut self) {
        while self.steps.len() > MAX_KEPT_REDEFINITIONS {
            let drop = match self.census_floor {
                // Nobody the census sees runs code this old.
                Some(floor) if self.census_may_drop_oldest(floor) => true,
                // A stale frame still needs it: only the hard caps.
                Some(_) => {
                    self.steps.len() > MAX_HISTORY_STEPS_HARD
                        || self.hard_footprint() > HISTORY_HARD_BYTE_BUDGET
                }
                // No census yet: the soft caps, as before wave 23.
                None => {
                    self.steps.len() > MAX_HISTORY_STEPS
                        || self.distinct_bytes() > HISTORY_BYTE_BUDGET
                }
            };
            if !drop {
                break;
            }
            let dropped = self.steps.remove(0);
            self.convertible_from = dropped.retired_at;
            self.dropped_steps = self.dropped_steps.wrapping_add(1);
        }
    }

    /// Should a merge whose plain tail is `tail` slots look for dead
    /// constants again (`merge_for_obsolete_code_compacting`)? Not while the
    /// last look found too few, no step was dropped since, and the tail has
    /// not doubled: only a dropped step makes a constant dead, and the
    /// threshold the look applies is relative to the tail (interpreter round
    /// i1 wave 25, lane L3).
    fn compaction_worth_a_look(&self, tail: usize) -> bool {
        let (fruitless_tail, dropped_then) = *self
            .fruitless_compaction
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fruitless_tail == 0
            || dropped_then != self.dropped_steps
            || tail >= fruitless_tail.saturating_mul(2)
    }

    /// Record the outcome of a look for dead constants at plain tail `tail`:
    /// `compacted` clears the record.
    fn note_compaction_look(&self, tail: usize, compacted: bool) {
        *self
            .fruitless_compaction
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = if compacted {
            (0, self.dropped_steps)
        } else {
            (tail, self.dropped_steps)
        };
    }

    /// `translation`, as the `Arc` of an equal one among the last
    /// [`SHARE_WINDOW`] steps when there is one.
    fn shared_translation(&self, translation: &[u16]) -> Arc<Translation> {
        let translation = Translation::from_dense(translation);
        self.steps
            .iter()
            .rev()
            .take(SHARE_WINDOW)
            .filter_map(|step| step.translation.as_ref())
            .find(|kept| ***kept == translation)
            .map_or_else(|| Arc::new(translation), Arc::clone)
    }

    /// `tables`, each sharing the `lines` of an equal table of the same
    /// method among the last [`SHARE_WINDOW`] steps when there is one.
    fn shared_line_tables(&self, mut tables: Vec<ReplacedLineTable>) -> Vec<ReplacedLineTable> {
        for table in &mut tables {
            let kept = self
                .steps
                .iter()
                .rev()
                .take(SHARE_WINDOW)
                .flat_map(|step| step.replaced_lines.iter())
                .find(|kept| {
                    kept.name == table.name
                        && kept.descriptor == table.descriptor
                        && kept.lines[..] == table.lines[..]
                });
            if let Some(kept) = kept {
                table.lines = Arc::clone(&kept.lines);
            }
        }
        tables
    }

    /// The bytes the history's translations and line tables take, each
    /// shared allocation counted once.
    fn distinct_bytes(&self) -> usize {
        // A set, not a list: a census-kept history may hold up to
        // `MAX_HISTORY_STEPS_HARD` steps (wave 23).
        let mut seen: FxHashSet<*const u8> = FxHashSet::default();
        let mut bytes = 0usize;
        for step in &self.steps {
            if let Some(translation) = &step.translation {
                if seen.insert(Arc::as_ptr(translation).cast::<u8>()) {
                    bytes += translation.byte_size();
                }
            }
            for table in step.replaced_lines.iter() {
                if seen.insert(table.lines.as_ptr().cast::<u8>()) {
                    bytes += table.lines.len() * std::mem::size_of::<LineNumberEntry>();
                }
            }
        }
        bytes
    }

    /// What the hard budget counts ([`HISTORY_HARD_BYTE_BUDGET`]): the
    /// distinct tables and every step's own record.
    fn hard_footprint(&self) -> usize {
        self.distinct_bytes()
            .saturating_add(self.steps.len().saturating_mul(HARD_STEP_RECORD_BYTES))
    }

    /// The line table of the body of `name` `descriptor` that a frame
    /// stamped `stamp` runs: `Some(None)` when it is the class's current
    /// method's (no redefinition since the stamp changed it), `Some(Some(t))`
    /// for a replaced table, and `None` when the frame is older than the kept
    /// history.
    ///
    /// The first step after the stamp that changed the method's table holds
    /// it: until that step every redefinition left it as it was.
    #[allow(clippy::option_option)]
    pub fn line_table_at(
        &self,
        stamp: u64,
        name: &str,
        descriptor: &str,
    ) -> Option<Option<Arc<[LineNumberEntry]>>> {
        if stamp < self.convertible_from {
            return None;
        }
        let replaced = self
            .steps
            .iter()
            .filter(|step| step.retired_at > stamp)
            .find_map(|step| {
                step.replaced_lines
                    .iter()
                    .find(|t| &*t.name == name && &*t.descriptor == descriptor)
            });
        Some(replaced.map(|t| Arc::clone(&t.lines)))
    }

    /// `retired_at` of the class's last redefinition: the stamp a frame
    /// translated into the current pool takes.
    #[inline]
    pub fn latest_redefinition(&self) -> u64 {
        self.latest
    }

    /// `class_redefinition_count()` just after the class's last redefinition
    /// began, before its pool was replaced. A frame stamped at or above it
    /// but below [`Self::latest_redefinition`] was built by a thread that did
    /// not hold the class-manager lock while the swap was under way, and its
    /// code may be either body: the invoke caches still validated the old
    /// one, the re-installed vtable snapshots already named the new one. Its
    /// stamp alone cannot say which (interpreter round i1 wave 23, lane L3;
    /// `obsolete_frames::convert_frames` compares its bytes).
    #[inline]
    pub fn latest_redefinition_began(&self) -> u64 {
        self.latest_began
    }

    /// Did the class's last redefinition leave some old constant-pool index
    /// naming a different constant, so that a frame still running the old
    /// body must be translated before it resolves another one? (When not,
    /// the old body reads the right constants as it is.)
    #[inline]
    pub fn last_redefinition_moved_constants(&self) -> bool {
        self.last_moved_constants
    }

    /// Was a frame stamped `stamp` built before the class's last
    /// redefinition, i.e. does it run code of a replaced body?
    #[inline]
    pub fn is_stale(&self, stamp: u64) -> bool {
        stamp < self.latest
    }

    /// `index`, an index into the pool of the code a frame stamped `stamp`
    /// runs, as an index into the class's current pool. `None` when the frame
    /// is older than the kept history or the slot names nothing.
    pub fn translate(&self, stamp: u64, index: u16) -> Option<u16> {
        if stamp < self.convertible_from {
            return None;
        }
        let mut index = index;
        for step in &self.steps {
            if step.retired_at > stamp {
                if let Some(translation) = &step.translation {
                    index = translation.get(index)?;
                }
                if index == 0 {
                    return None;
                }
            }
        }
        Some(index)
    }
}

/// A key equal for exactly the constant-pool entries that name the same
/// constant, whatever their indices: each component is replaced by its own
/// key, length-prefixed so no two different entries share one.
fn content_key(
    pool: &ConstantPool,
    bsms: &[BootstrapMethod],
    index: u16,
    memo: &mut [Option<Option<Arc<str>>>],
    depth: u32,
) -> Option<Arc<str>> {
    let slot = usize::from(index);
    if let Some(Some(known)) = memo.get(slot) {
        return known.clone();
    }
    if depth > MAX_KEY_DEPTH {
        return None;
    }
    let key = content_key_uncached(pool, bsms, index, memo, depth);
    if let Some(cell) = memo.get_mut(slot) {
        *cell = Some(key.clone());
    }
    key
}

fn content_key_uncached(
    pool: &ConstantPool,
    bsms: &[BootstrapMethod],
    index: u16,
    memo: &mut [Option<Option<Arc<str>>>],
    depth: u32,
) -> Option<Arc<str>> {
    use ConstantPoolEntry as E;
    let mut key = String::new();
    let part = |key: &mut String, index: u16, memo: &mut [Option<Option<Arc<str>>>]| {
        push_key_part(key, content_key(pool, bsms, index, memo, depth + 1)?);
        Some(())
    };
    match pool.get(index)? {
        E::Tombstone => return None,
        E::Utf8(s) => {
            let _ = write!(key, "U{}:", s.len());
            key.push_str(s);
            if let Some(units) = pool.get_utf8_wide(index) {
                key.push('W');
                for unit in units {
                    let _ = write!(key, "{unit:04x}");
                }
            }
        }
        E::Integer(v) => {
            let _ = write!(key, "I{v}");
        }
        E::Float(v) => {
            let _ = write!(key, "F{:08x}", v.to_bits());
        }
        E::Long(v) => {
            let _ = write!(key, "J{v}");
        }
        E::Double(v) => {
            let _ = write!(key, "D{:016x}", v.to_bits());
        }
        E::ClassReference { name_index } => {
            key.push('C');
            part(&mut key, *name_index, &mut *memo)?;
        }
        E::StringReference { string_index } => {
            key.push('S');
            part(&mut key, *string_index, &mut *memo)?;
        }
        E::FieldReference {
            class_index,
            name_and_type_index,
        } => {
            key.push('f');
            part(&mut key, *class_index, &mut *memo)?;
            part(&mut key, *name_and_type_index, &mut *memo)?;
        }
        E::MethodReference {
            class_index,
            name_and_type_index,
        } => {
            key.push('m');
            part(&mut key, *class_index, &mut *memo)?;
            part(&mut key, *name_and_type_index, &mut *memo)?;
        }
        E::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => {
            key.push('i');
            part(&mut key, *class_index, &mut *memo)?;
            part(&mut key, *name_and_type_index, &mut *memo)?;
        }
        E::NameAndType {
            name_index,
            descriptor_index,
        } => {
            key.push('N');
            part(&mut key, *name_index, &mut *memo)?;
            part(&mut key, *descriptor_index, &mut *memo)?;
        }
        E::MethodHandle {
            reference_kind,
            reference_index,
        } => {
            let _ = write!(key, "H{reference_kind}:");
            part(&mut key, *reference_index, &mut *memo)?;
        }
        E::MethodType { descriptor_index } => {
            key.push('T');
            part(&mut key, *descriptor_index, &mut *memo)?;
        }
        E::Dynamic {
            bootstrap_method_attr_index,
            name_and_type_index,
        } => {
            key.push('Y');
            push_key_part(
                &mut key,
                bsm_key(
                    pool,
                    bsms,
                    *bootstrap_method_attr_index,
                    &mut *memo,
                    depth + 1,
                )?,
            );
            part(&mut key, *name_and_type_index, &mut *memo)?;
        }
        E::InvokeDynamic {
            bootstrap_method_attr_index,
            name_and_type_index,
        } => {
            key.push('Z');
            push_key_part(
                &mut key,
                bsm_key(
                    pool,
                    bsms,
                    *bootstrap_method_attr_index,
                    &mut *memo,
                    depth + 1,
                )?,
            );
            part(&mut key, *name_and_type_index, &mut *memo)?;
        }
        E::Module { name_index } => {
            key.push('M');
            part(&mut key, *name_index, &mut *memo)?;
        }
        E::Package { name_index } => {
            key.push('P');
            part(&mut key, *name_index, &mut *memo)?;
        }
    }
    Some(Arc::from(key))
}

/// [`content_key`] of a `BootstrapMethods` entry: its method handle and its
/// static arguments.
fn bsm_key(
    pool: &ConstantPool,
    bsms: &[BootstrapMethod],
    index: u16,
    memo: &mut [Option<Option<Arc<str>>>],
    depth: u32,
) -> Option<Arc<str>> {
    if depth > MAX_KEY_DEPTH {
        return None;
    }
    let bsm = bsms.get(usize::from(index))?;
    let mut key = String::from("B");
    let arguments = bsm.bootstrap_arguments.iter().copied();
    for index in std::iter::once(bsm.bootstrap_method_ref).chain(arguments) {
        push_key_part(&mut key, content_key(pool, bsms, index, memo, depth + 1)?);
    }
    Some(Arc::from(key))
}

/// Append `part` to `key`, length-prefixed.
fn push_key_part(key: &mut String, part: Arc<str>) {
    let _ = write!(key, "{}:", part.len());
    key.push_str(&part);
}

/// The state of one [`merge_for_obsolete_code`].
struct Merger<'a> {
    old: &'a ConstantPool,
    old_bsms: &'a [BootstrapMethod],
    entries: Vec<ConstantPoolEntry>,
    wide: HashMap<u16, Arc<[u16]>>,
    bsms: Vec<BootstrapMethod>,
    /// Content key -> lowest merged index holding it.
    by_key: FxHashMap<Arc<str>, u16>,
    bsm_by_key: FxHashMap<Arc<str>, u16>,
    old_keys: Vec<Option<Option<Arc<str>>>>,
    translation: Vec<u16>,
    bsm_translation: Vec<Option<u16>>,
    failed: bool,
}

impl<'a> Merger<'a> {
    fn new(
        old: &'a ConstantPool,
        old_bsms: &'a [BootstrapMethod],
        new: &ConstantPool,
        new_bsms: &[BootstrapMethod],
    ) -> Self {
        let mut entries = Vec::with_capacity(new.len() + 16);
        let mut wide = HashMap::new();
        let mut new_keys: Vec<Option<Option<Arc<str>>>> = vec![None; new.len()];
        let mut by_key: FxHashMap<Arc<str>, u16> = FxHashMap::default();
        for slot in 0..new.len() {
            let Ok(index) = u16::try_from(slot) else {
                break;
            };
            entries.push(
                new.get(index)
                    .cloned()
                    .unwrap_or(ConstantPoolEntry::Tombstone),
            );
            if let Some(units) = new.get_utf8_wide(index) {
                wide.insert(index, Arc::from(units));
            }
            if index != 0 {
                if let Some(key) = content_key(new, new_bsms, index, &mut new_keys, 0) {
                    by_key.entry(key).or_insert(index);
                }
            }
        }
        let mut bsm_by_key: FxHashMap<Arc<str>, u16> = FxHashMap::default();
        for slot in 0..new_bsms.len() {
            let Ok(index) = u16::try_from(slot) else {
                break;
            };
            if let Some(key) = bsm_key(new, new_bsms, index, &mut new_keys, 0) {
                bsm_by_key.entry(key).or_insert(index);
            }
        }
        Self {
            old,
            old_bsms,
            entries,
            wide,
            bsms: new_bsms.to_vec(),
            by_key,
            bsm_by_key,
            old_keys: vec![None; old.len()],
            translation: vec![0; old.len()],
            bsm_translation: vec![None; old_bsms.len()],
            failed: false,
        }
    }

    /// The merged index of old entry `index`, appending it (after its
    /// components) when the merged pool has no equal. `for_ldc`: the index
    /// must fit a one-byte `ldc` operand if an append can still make it.
    /// `None` for a slot that names nothing, or on failure (`self.failed`).
    fn intern_old(&mut self, index: u16, for_ldc: bool) -> Option<u16> {
        let slot = usize::from(index);
        let entry = match self.old.get(index) {
            None | Some(ConstantPoolEntry::Tombstone) => return None,
            Some(entry) => entry.clone(),
        };
        let wants_low = for_ldc && self.entries.len() <= usize::from(u8::MAX);
        let fits = |merged: u16| !wants_low || merged <= u16::from(u8::MAX);
        let known = self.translation.get(slot).copied().unwrap_or(0);
        if known != 0 && fits(known) {
            return Some(known);
        }
        let key = content_key(self.old, self.old_bsms, index, &mut self.old_keys, 0);
        if let Some(merged) = key.as_ref().and_then(|k| self.by_key.get(k).copied()) {
            if fits(merged) {
                if let Some(cell) = self.translation.get_mut(slot) {
                    *cell = merged;
                }
                return Some(merged);
            }
        }
        // Append. The entry takes its slot BEFORE its components are
        // translated, so an `ldc` constant lands low even when its UTF-8
        // has to be appended too, and a (malformed) cycle ends at the slot.
        let two_slots = matches!(
            entry,
            ConstantPoolEntry::Long(_) | ConstantPoolEntry::Double(_)
        );
        let at = self.entries.len();
        let Ok(at16) = u16::try_from(at) else {
            self.failed = true;
            return None;
        };
        if at + 1 + usize::from(two_slots) > MAX_POOL_COUNT {
            self.failed = true;
            return None;
        }
        self.entries.push(ConstantPoolEntry::Tombstone);
        if two_slots {
            self.entries.push(ConstantPoolEntry::Tombstone);
        }
        if let Some(key) = key {
            let lowest = self.by_key.entry(key).or_insert(at16);
            if at16 < *lowest {
                *lowest = at16;
            }
        }
        if let Some(cell) = self.translation.get_mut(slot) {
            *cell = at16;
        }
        let merged = self.translate_entry(entry, index, at16)?;
        if let Some(cell) = self.entries.get_mut(at) {
            *cell = merged;
        }
        Some(at16)
    }

    /// A component index of an old entry, which must name something.
    fn component(&mut self, index: u16) -> Option<u16> {
        let merged = self.intern_old(index, false);
        if merged.is_none() {
            self.failed = true;
        }
        merged
    }

    /// Old entry `entry` (at old index `index`, appended at `at`) with its
    /// components translated.
    fn translate_entry(
        &mut self,
        entry: ConstantPoolEntry,
        index: u16,
        at: u16,
    ) -> Option<ConstantPoolEntry> {
        use ConstantPoolEntry as E;
        Some(match entry {
            E::Tombstone => {
                self.failed = true;
                return None;
            }
            E::Utf8(s) => {
                if let Some(units) = self.old.get_utf8_wide(index) {
                    self.wide.insert(at, Arc::from(units));
                }
                E::Utf8(s)
            }
            plain @ (E::Integer(_) | E::Float(_) | E::Long(_) | E::Double(_)) => plain,
            E::ClassReference { name_index } => E::ClassReference {
                name_index: self.component(name_index)?,
            },
            E::StringReference { string_index } => E::StringReference {
                string_index: self.component(string_index)?,
            },
            E::FieldReference {
                class_index,
                name_and_type_index,
            } => E::FieldReference {
                class_index: self.component(class_index)?,
                name_and_type_index: self.component(name_and_type_index)?,
            },
            E::MethodReference {
                class_index,
                name_and_type_index,
            } => E::MethodReference {
                class_index: self.component(class_index)?,
                name_and_type_index: self.component(name_and_type_index)?,
            },
            E::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            } => E::InterfaceMethodReference {
                class_index: self.component(class_index)?,
                name_and_type_index: self.component(name_and_type_index)?,
            },
            E::NameAndType {
                name_index,
                descriptor_index,
            } => E::NameAndType {
                name_index: self.component(name_index)?,
                descriptor_index: self.component(descriptor_index)?,
            },
            E::MethodHandle {
                reference_kind,
                reference_index,
            } => E::MethodHandle {
                reference_kind,
                reference_index: self.component(reference_index)?,
            },
            E::MethodType { descriptor_index } => E::MethodType {
                descriptor_index: self.component(descriptor_index)?,
            },
            E::Dynamic {
                bootstrap_method_attr_index,
                name_and_type_index,
            } => E::Dynamic {
                bootstrap_method_attr_index: self.intern_bsm(bootstrap_method_attr_index)?,
                name_and_type_index: self.component(name_and_type_index)?,
            },
            E::InvokeDynamic {
                bootstrap_method_attr_index,
                name_and_type_index,
            } => E::InvokeDynamic {
                bootstrap_method_attr_index: self.intern_bsm(bootstrap_method_attr_index)?,
                name_and_type_index: self.component(name_and_type_index)?,
            },
            E::Module { name_index } => E::Module {
                name_index: self.component(name_index)?,
            },
            E::Package { name_index } => E::Package {
                name_index: self.component(name_index)?,
            },
        })
    }

    /// The merged `BootstrapMethods` index of old entry `index`.
    fn intern_bsm(&mut self, index: u16) -> Option<u16> {
        let slot = usize::from(index);
        let Some(bsm) = self.old_bsms.get(slot).cloned() else {
            self.failed = true;
            return None;
        };
        if let Some(Some(known)) = self.bsm_translation.get(slot) {
            return Some(*known);
        }
        let key = bsm_key(self.old, self.old_bsms, index, &mut self.old_keys, 0);
        if let Some(merged) = key.as_ref().and_then(|k| self.bsm_by_key.get(k).copied()) {
            if let Some(cell) = self.bsm_translation.get_mut(slot) {
                *cell = Some(merged);
            }
            return Some(merged);
        }
        let at = self.bsms.len();
        let Ok(at16) = u16::try_from(at) else {
            self.failed = true;
            return None;
        };
        self.bsms.push(BootstrapMethod {
            bootstrap_method_ref: 0,
            bootstrap_arguments: Vec::new(),
        });
        if let Some(key) = key {
            self.bsm_by_key.entry(key).or_insert(at16);
        }
        if let Some(cell) = self.bsm_translation.get_mut(slot) {
            *cell = Some(at16);
        }
        let bootstrap_method_ref = self.component(bsm.bootstrap_method_ref)?;
        let mut bootstrap_arguments = Vec::with_capacity(bsm.bootstrap_arguments.len());
        for &argument in &bsm.bootstrap_arguments {
            bootstrap_arguments.push(self.component(argument)?);
        }
        if let Some(cell) = self.bsms.get_mut(at) {
            *cell = BootstrapMethod {
                bootstrap_method_ref,
                bootstrap_arguments,
            };
        }
        Some(at16)
    }

    fn finish(self) -> Option<MergedPool> {
        if self.failed || self.entries.len() > MAX_POOL_COUNT {
            return None;
        }
        let constant_pool = if self.wide.is_empty() {
            ConstantPool::new(self.entries)
        } else {
            ConstantPool::new_with_wide(self.entries, self.wide)
        };
        Some(MergedPool {
            constant_pool,
            bootstrap_methods: self.bsms,
            translation: self.translation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_reader::constant_pool::ConstantPoolEntry as E;

    fn utf8(s: &str) -> ConstantPoolEntry {
        E::Utf8(Arc::from(s))
    }

    /// `#1 "Holder"`, `#2 Class #1`, then `#3` / `#4` / `#5` as given.
    fn pool(tail: Vec<ConstantPoolEntry>) -> ConstantPool {
        let mut entries = vec![
            E::Tombstone,
            utf8("Holder"),
            E::ClassReference { name_index: 1 },
        ];
        entries.extend(tail);
        ConstantPool::new(entries)
    }

    fn string_at(pool: &ConstantPool, index: u16) -> Option<&str> {
        match pool.get(index)? {
            E::StringReference { string_index } => pool.get_utf8(*string_index),
            _ => None,
        }
    }

    /// The case the old code breaks on: the same index names a different
    /// string in the new pool. The old string is appended (with its UTF-8)
    /// and the translation points the old index at it; the new index keeps
    /// its new meaning.
    #[test]
    fn a_renumbered_string_is_appended_and_translated() {
        let old = pool(vec![utf8("old"), E::StringReference { string_index: 3 }]);
        let new = pool(vec![utf8("new"), E::StringReference { string_index: 3 }]);
        let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[4]).expect("fits");
        assert_eq!(string_at(&merged.constant_pool, 4), Some("new"));
        let moved = merged.translation[4];
        assert!(moved >= 5, "appended after the new pool, got #{moved}");
        assert_eq!(string_at(&merged.constant_pool, moved), Some("old"));
        // Unchanged entries map to themselves.
        assert_eq!(merged.translation[1], 1);
        assert_eq!(merged.translation[2], 2);
    }

    /// An identical pool (a retransform whose transformer returned the same
    /// bytes) appends nothing.
    #[test]
    fn an_identical_pool_appends_nothing() {
        let old = pool(vec![utf8("same"), E::StringReference { string_index: 3 }]);
        let new = pool(vec![utf8("same"), E::StringReference { string_index: 3 }]);
        let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[]).expect("fits");
        assert_eq!(merged.constant_pool.len(), new.len());
        assert_eq!(merged.translation, vec![0, 1, 2, 3, 4]);
    }

    /// A constant that only moved is found at its new index, not appended.
    #[test]
    fn a_moved_constant_is_deduplicated() {
        let old = pool(vec![utf8("x"), E::StringReference { string_index: 3 }]);
        let new = pool(vec![
            utf8("pad"),
            utf8("x"),
            E::StringReference { string_index: 4 },
        ]);
        let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[4]).expect("fits");
        assert_eq!(merged.translation[4], 5);
        assert_eq!(merged.constant_pool.len(), new.len());
    }

    /// A `long` takes two slots when appended, and a member reference is
    /// rebuilt from translated components.
    #[test]
    fn longs_and_member_references_keep_their_shape() {
        let old = pool(vec![
            E::Long(7),
            E::Tombstone,
            utf8("f"),
            utf8("I"),
            E::NameAndType {
                name_index: 5,
                descriptor_index: 6,
            },
            E::FieldReference {
                class_index: 2,
                name_and_type_index: 7,
            },
        ]);
        let new = pool(vec![E::Long(8), E::Tombstone]);
        let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[]).expect("fits");
        let long_at = merged.translation[3];
        assert!(matches!(
            merged.constant_pool.get(long_at),
            Some(E::Long(7))
        ));
        assert!(matches!(
            merged.constant_pool.get(long_at + 1),
            Some(E::Tombstone)
        ));
        assert_eq!(
            merged.translation[4], 0,
            "a long's second slot names nothing"
        );
        let field_at = merged.translation[8];
        match merged.constant_pool.get(field_at) {
            Some(E::FieldReference {
                class_index,
                name_and_type_index,
            }) => {
                assert_eq!(
                    merged.constant_pool.get_class_name(*class_index),
                    Some("Holder")
                );
                assert_eq!(
                    merged.constant_pool.get_name_and_type(*name_and_type_index),
                    Some(("f", "I"))
                );
            }
            other => unreachable!("expected a field reference, got {other:?}"),
        }
    }

    /// An old `invokedynamic` brings its bootstrap method along, rebuilt
    /// from translated arguments, when the new class has no equal one.
    #[test]
    fn an_old_call_site_brings_its_bootstrap_method() {
        let old = pool(vec![
            utf8("bsm"),
            utf8("()V"),
            E::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            E::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            },
            E::MethodHandle {
                reference_kind: 6,
                reference_index: 6,
            },
            utf8("arg-old"),
            E::StringReference { string_index: 8 },
            E::InvokeDynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 5,
            },
        ]);
        let old_bsms = vec![BootstrapMethod {
            bootstrap_method_ref: 7,
            bootstrap_arguments: vec![9],
        }];
        let new = pool(vec![utf8("unrelated")]);
        let merged = merge_for_obsolete_code(&old, &old_bsms, &new, &[], &[]).expect("fits");
        let site = merged.translation[10];
        let Some(E::InvokeDynamic {
            bootstrap_method_attr_index,
            ..
        }) = merged.constant_pool.get(site)
        else {
            unreachable!("the call site was appended");
        };
        let bsm = &merged.bootstrap_methods[usize::from(*bootstrap_method_attr_index)];
        assert_eq!(bsm.bootstrap_method_ref, merged.translation[7]);
        assert_eq!(bsm.bootstrap_arguments, vec![merged.translation[9]]);
        assert_eq!(
            string_at(&merged.constant_pool, merged.translation[9]),
            Some("arg-old")
        );
    }

    /// An `ldc` constant is placed before its UTF-8, so it gets the lower
    /// index; the rewrite of `ldc` refuses an index above 255.
    #[test]
    fn ldc_constants_are_placed_first_and_must_fit_a_byte() {
        let old = pool(vec![utf8("gone"), E::StringReference { string_index: 3 }]);
        let new = pool(vec![utf8("fresh")]);
        let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[4]).expect("fits");
        assert_eq!(
            merged.translation[4], 4,
            "the String takes the first free slot"
        );
        assert_eq!(string_at(&merged.constant_pool, 4), Some("gone"));

        let map = |i: u16| Some(i + 1);
        assert_eq!(
            translate_code(&[0x12, 4, 0xb0], &map),
            Some(vec![0x12, 5, 0xb0])
        );
        let high = |_: u16| Some(300u16);
        assert_eq!(translate_code(&[0x12, 4, 0xb0], &high), None);
    }

    /// Every constant-pool operand is rewritten; nothing else moves,
    /// including a `tableswitch` whose padding depends on its pc.
    #[test]
    fn translate_code_rewrites_operands_in_place() {
        let code = vec![
            0x13, 0x00, 0x10, // ldc_w #16
            0xb2, 0x00, 0x11, // getstatic #17
            0xb9, 0x00, 0x12, 0x02, 0x00, // invokeinterface #18 count 2
            0xba, 0x00, 0x13, 0x00, 0x00, // invokedynamic #19
            0x03, // iconst_0
            0xaa, 0x00, 0x00, // tableswitch, padded to pc 20
            0x00, 0x00, 0x00, 0x10, // default
            0x00, 0x00, 0x00, 0x00, // low
            0x00, 0x00, 0x00, 0x00, // high
            0x00, 0x00, 0x00, 0x10, // offset
            0xc0, 0x00, 0x14, // checkcast #20
            0xb1, // return
        ];
        let map = |i: u16| Some(i + 0x100);
        let out = translate_code(&code, &map).expect("translates");
        assert_eq!(out.len(), code.len());
        assert_eq!(&out[0..3], &[0x13, 0x01, 0x10]);
        assert_eq!(&out[3..6], &[0xb2, 0x01, 0x11]);
        assert_eq!(&out[6..11], &[0xb9, 0x01, 0x12, 0x02, 0x00]);
        assert_eq!(&out[11..16], &[0xba, 0x01, 0x13, 0x00, 0x00]);
        assert_eq!(&out[16..36], &code[16..36], "the switch is untouched");
        assert_eq!(&out[36..39], &[0xc0, 0x01, 0x14]);
        let unmapped = |_: u16| None;
        assert_eq!(translate_code(&code, &unmapped), None);
    }

    #[test]
    fn exception_tables_translate_catch_types_but_not_finally() {
        let table = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 4,
                handler_pc: 5,
                catch_type: 7,
            },
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 4,
                handler_pc: 9,
                catch_type: 0,
            },
        ];
        let map = |i: u16| Some(i * 2);
        let out = translate_exception_table(&table, &map).expect("translates");
        assert_eq!(out[0].catch_type, 14);
        assert_eq!(out[1].catch_type, 0);
        assert_eq!(out[0].handler_pc, 5);
    }

    /// A frame is translated through every redefinition after its stamp,
    /// in order; one older than the kept history, or than a redefinition
    /// that recorded no translation, is not.
    #[test]
    fn the_history_composes_steps_after_the_stamp() {
        let mut history = RedefinitionHistory::default();
        assert!(!history.last_redefinition_moved_constants());
        history.record(5, Some(vec![0, 1, 2, 0]), Vec::new());
        assert!(
            !history.last_redefinition_moved_constants(),
            "every old index kept its constant"
        );
        history.record(10, Some(vec![0, 5, 6]), Vec::new());
        assert!(history.last_redefinition_moved_constants());
        history.record(20, Some(vec![0, 0, 0, 0, 0, 7, 8]), Vec::new());
        assert!(history.is_stale(9));
        assert!(history.is_stale(19));
        assert!(!history.is_stale(20));
        assert_eq!(history.translate(9, 1), Some(7), "both steps");
        assert_eq!(history.translate(15, 5), Some(7), "the second step only");
        assert_eq!(
            history.translate(25, 3),
            Some(3),
            "current code maps to itself"
        );
        assert_eq!(history.translate(9, 0), None);
        assert_eq!(history.latest_redefinition(), 20);

        history.record(30, None, Vec::new());
        assert!(
            !history.last_redefinition_moved_constants(),
            "nothing to repair with"
        );
        assert_eq!(
            history.translate(25, 3),
            None,
            "the pool was replaced without a map"
        );
        assert_eq!(history.translate(30, 3), Some(3));

        // Each step swaps #1 and #2, so an even number of them is the
        // identity. Each is also a different 2100-entry map (one more slot
        // names nothing) that swaps every later pair too, so no two
        // neighbours share a run (wave 24: translations are kept as runs)
        // and eight of them are over `HISTORY_BYTE_BUDGET`: a ninth drops
        // the first.
        let mut capped = RedefinitionHistory::default();
        for i in 0..=MAX_KEPT_REDEFINITIONS as u64 {
            let mut translation: Vec<u16> = (0..2100).collect();
            translation.swap(1, 2);
            for pair in (3..2099).step_by(2) {
                translation.swap(pair, pair + 1);
            }
            translation[10 + i as usize] = 0;
            capped.record(100 + i, Some(translation), Vec::new());
        }
        assert!(capped.distinct_bytes() > HISTORY_BYTE_BUDGET);
        assert_eq!(capped.translate(99, 1), None, "its first step was dropped");
        assert_eq!(capped.translate(100, 1), Some(1));
        assert_eq!(capped.translate(101, 1), Some(2));
    }

    /// Interpreter round i1 wave 22, lane L3 (i19-L3 page, case 2): steps
    /// that repeat a translation share it, so a history of 200 such steps
    /// fits the byte budget and a frame older than all of them is still
    /// translated; the step cap still bounds the count.
    #[test]
    fn repeated_translations_are_shared_and_kept() {
        let mut toggled = RedefinitionHistory::default();
        for i in 0..200u64 {
            toggled.record(100 + i, Some(vec![0, 2, 1]), Vec::new());
        }
        assert_eq!(toggled.steps.len(), 200);
        assert!(toggled.steps.iter().all(|step| {
            step.translation
                .as_ref()
                .zip(toggled.steps[0].translation.as_ref())
                .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
        }));
        // Three runs (#0 names nothing, #1 -> #2, #2 -> #1), kept once.
        assert_eq!(
            toggled.distinct_bytes(),
            3 * std::mem::size_of::<TranslationRun>()
        );
        assert_eq!(toggled.translate(99, 1), Some(1), "200 swaps");
        assert_eq!(toggled.translate(100, 1), Some(2), "199 swaps");

        let mut capped = RedefinitionHistory::default();
        for i in 0..=MAX_HISTORY_STEPS as u64 {
            capped.record(100 + i, Some(vec![0, 2, 1]), Vec::new());
        }
        assert_eq!(capped.steps.len(), MAX_HISTORY_STEPS);
        assert_eq!(capped.translate(99, 1), None, "past the step cap");
        assert_eq!(capped.translate(100, 1), Some(1));
    }

    /// Interpreter round i1 wave 23, lane L3 (the i22-L3 census proposal):
    /// once a stale-frame census has run, steps no frame needs are dropped
    /// past the last eight whatever they cost, and steps a stale frame still
    /// needs outlive the soft caps, up to the hard ones.
    #[test]
    fn the_census_drops_unneeded_steps_and_keeps_needed_ones() {
        // Each step swaps #1 and #2 and names nothing at one more slot, a
        // different one than the last four steps' (so none is shared); it
        // swaps every later pair too, so each of its 64 slots is a run of
        // its own (wave 24: translations are kept as runs).
        let distinct = |i: u64| {
            let mut translation: Vec<u16> = (0..64).collect();
            translation.swap(1, 2);
            for pair in (3..63).step_by(2) {
                translation.swap(pair, pair + 1);
            }
            translation[3 + (i % 50) as usize] = 0;
            translation
        };
        let mut kept = RedefinitionHistory::default();
        // A thread parked since count 50 still has its frames there.
        kept.prune(50);
        for i in 0..300u64 {
            kept.record(100 + i, Some(distinct(i)), Vec::new());
        }
        assert!(kept.distinct_bytes() > HISTORY_BYTE_BUDGET);
        assert_eq!(kept.steps.len(), 300, "needed: past both soft caps");
        assert_eq!(kept.translate(99, 1), Some(1), "300 swaps");

        // Every thread has moved its frames past count 350: the steps
        // retired up to it translate for nobody.
        kept.prune(350);
        assert_eq!(kept.steps.len(), 49, "retired 351..=399");
        assert_eq!(kept.translate(99, 1), None);
        assert_eq!(kept.translate(350, 1), Some(2), "49 swaps");

        kept.prune(1_000);
        assert_eq!(
            kept.steps.len(),
            MAX_KEPT_REDEFINITIONS,
            "the last eight stay whatever the census says"
        );

        let mut hard = RedefinitionHistory::default();
        hard.prune(0);
        for i in 0..=MAX_HISTORY_STEPS_HARD as u64 {
            hard.record(100 + i, Some(vec![0, 2, 1]), Vec::new());
        }
        assert_eq!(hard.steps.len(), MAX_HISTORY_STEPS_HARD, "the hard cap");
        assert_eq!(hard.translate(99, 1), None);
    }

    /// Interpreter round i1 wave 24, lane L3: steps that replaced line tables
    /// are what a stored backtrace reads its version's lines from, and a
    /// backtrace is not a frame the census sees, so the census drops them
    /// only past the soft caps; steps without line tables go at once.
    #[test]
    fn the_census_keeps_line_tables_within_the_soft_caps() {
        let mut history = RedefinitionHistory::default();
        history.prune(0);
        for i in 0..20u64 {
            let line = u16::try_from(i).unwrap_or(0);
            history.record(100 + i, Some(vec![0, 2, 1]), vec![replaced("run", &[(0, line)])]);
        }
        history.prune(1_000);
        assert_eq!(history.steps.len(), 20, "kept for stored backtraces");
        assert_eq!(
            history
                .line_table_at(99, "run", "()V")
                .map(|t| t.map(|t| line_number_at(&t, 0))),
            Some(Some(Some(0))),
            "the first version's lines"
        );

        let mut plain = RedefinitionHistory::default();
        plain.prune(0);
        for i in 0..20u64 {
            plain.record(100 + i, Some(vec![0, 2, 1]), Vec::new());
        }
        plain.prune(1_000);
        assert_eq!(plain.steps.len(), MAX_KEPT_REDEFINITIONS, "no lines: dropped");
    }

    /// A redefinition records the count its swap began at: a frame stamped
    /// in between is stale by its stamp, and `latest_redefinition_began`
    /// tells the VM its body may be either one.
    #[test]
    fn a_redefinition_window_is_recorded() {
        let mut history = RedefinitionHistory::default();
        history.record_window(7, 9, Some(vec![0, 2, 1]), Vec::new(), None);
        assert_eq!(history.latest_redefinition_began(), 7);
        assert_eq!(history.latest_redefinition(), 9);
        assert!(history.is_stale(8));
        assert!(!history.is_stale(9));
        history.record(12, Some(vec![0, 2, 1]), Vec::new());
        assert_eq!(history.latest_redefinition_began(), 12, "no window");
    }

    /// A redefinition that moved no constant and changed no line table (an
    /// identical retransform) records no step: nine of them after a moving
    /// one leave that one reachable, where each used to take a slot and the
    /// ninth dropped it (i19-L3 page, case 2). A frame built before any of
    /// them is still stale, and translated through the moving step alone.
    #[test]
    fn identical_redefinitions_take_no_history_slot() {
        let mut history = RedefinitionHistory::default();
        history.record(10, Some(vec![0, 3, 2, 1]), Vec::new());
        for i in 0..=MAX_KEPT_REDEFINITIONS as u64 {
            history.record(20 + i, Some(vec![0, 1, 2, 3]), Vec::new());
        }
        assert_eq!(history.latest_redefinition(), 28);
        assert!(!history.last_redefinition_moved_constants());
        assert!(history.is_stale(5));
        assert_eq!(history.translate(5, 1), Some(3), "the moving step");
        assert_eq!(history.translate(15, 1), Some(1), "identities only");
        assert_eq!(history.translate(5, 0), None, "slot 0 names nothing");
    }

    fn lines(pairs: &[(u16, u16)]) -> Vec<LineNumberEntry> {
        pairs
            .iter()
            .map(|&(start_pc, line_number)| LineNumberEntry {
                start_pc,
                line_number,
            })
            .collect()
    }

    fn replaced(name: &str, pairs: &[(u16, u16)]) -> ReplacedLineTable {
        ReplacedLineTable {
            name: Arc::from(name),
            descriptor: Arc::from("()V"),
            lines: Arc::from(lines(pairs)),
        }
    }

    /// A frame's line table is the one the first step after its stamp that
    /// changed the method replaced; with none, the current method's; older
    /// than the kept history, unknown. A step that only changed lines is
    /// kept even though its pool translation is the identity.
    #[test]
    fn line_tables_are_found_for_the_body_a_stamp_names() {
        let mut history = RedefinitionHistory::default();
        history.record(10, Some(vec![0, 1]), vec![replaced("run", &[(0, 5)])]);
        history.record(20, Some(vec![0, 1]), Vec::new());
        history.record(30, Some(vec![0, 1]), vec![replaced("run", &[(0, 7)])]);
        assert!(!history.last_redefinition_moved_constants());
        let first_line = |stamp: u64| {
            history
                .line_table_at(stamp, "run", "()V")
                .map(|t| t.map(|t| line_number_at(&t, 0)))
        };
        assert_eq!(first_line(5), Some(Some(Some(5))), "the original body");
        assert_eq!(first_line(15), Some(Some(Some(7))), "the second body");
        assert_eq!(first_line(25), Some(Some(Some(7))), "unchanged at 20");
        assert_eq!(first_line(35), Some(None), "the current body");
        assert_eq!(
            history
                .line_table_at(5, "other", "()V")
                .map(|t| t.is_some()),
            Some(false),
            "a method no step changed"
        );
        history.record(40, None, Vec::new());
        assert!(history.line_table_at(35, "run", "()V").is_none());
    }

    /// The largest start at or below the bci wins, the last of equals; a
    /// bci before the first entry has no line.
    #[test]
    fn line_number_at_follows_the_line_number_table_rule() {
        let table = lines(&[(0, 10), (4, 11), (4, 12), (9, 13)]);
        assert_eq!(line_number_at(&table, 0), Some(10));
        assert_eq!(line_number_at(&table, 3), Some(10));
        assert_eq!(line_number_at(&table, 4), Some(12));
        assert_eq!(line_number_at(&table, 100), Some(13));
        assert_eq!(line_number_at(&lines(&[(2, 1)]), 1), None);
        assert_eq!(line_number_at(&[], 0), None);
    }

    /// Only the methods whose table differs are kept, as they were.
    #[test]
    fn replaced_line_tables_keep_only_changed_methods() {
        use cratonvm_reader::attribute::{CodeAttribute, LazyAttribute};
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        let method = |name: &str, pairs: &[(u16, u16)]| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from(name),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 0,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
                exception_table: Vec::new(),
                attributes: vec![Attribute::LineNumberTable(lines(pairs))],
            }))],
        };
        let old = vec![method("same", &[(0, 3)]), method("moved", &[(0, 8)])];
        let new = vec![method("same", &[(0, 3)]), method("moved", &[(0, 9)])];
        let kept = replaced_line_tables(&old, &new);
        assert_eq!(kept.len(), 1);
        assert_eq!(&*kept[0].name, "moved");
        assert_eq!(line_number_at(&kept[0].lines, 0), Some(8));
    }

    /// An `ldc` whose constant maps above 255 keeps its byte and is
    /// reported; a second translation reads a reported site's index, not
    /// its stale byte, and writes the byte back once the index fits.
    #[test]
    fn translate_code_widening_reports_ldc_sites_above_a_byte() {
        let code = [0x12, 4, 0x12, 5, 0xb1];
        let map = |i: u16| Some(if i == 5 { 300 } else { i + 1 });
        let (out, widened) = translate_code_widening(&code, &map, &[]).expect("maps");
        assert_eq!(out, vec![0x12, 5, 0x12, 5, 0xb1]);
        assert_eq!(widened, vec![(2, 300)]);
        assert_eq!(translate_code(&code, &map), None);
        // The next redefinition moves #300 down to #7.
        let down = |i: u16| Some(if i == 300 { 7 } else { i });
        let (again, still) = translate_code_widening(&out, &down, &widened).expect("maps");
        assert_eq!(again, vec![0x12, 5, 0x12, 7, 0xb1]);
        assert!(still.is_empty());
    }

    /// A version-52 class `obsolete/Probe` whose one method `static value()I`
    /// is `ldc #8; ireturn`, `#8` being `CONSTANT_Integer constant`.
    fn ldc_class(constant: i32) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, "obsolete/Probe"); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.push(3); // #8 Integer
        b.extend_from_slice(&constant.to_be_bytes());
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
        let code = [0x12, 8, 0xac]; // ldc #8; ireturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    /// `ClassManager::redefine_class` installs the merged pool and records
    /// the step: the new code's `#8` keeps the new constant, and the old
    /// body's `ldc #8` rewrites to the index of the old one.
    #[test]
    fn a_redefinition_keeps_the_old_constant_reachable_through_the_history() {
        use crate::{ClassLoaderId, ClassManager, DefineClassOptions, RedefineOptions};
        let mut cm = ClassManager::new(&[], &[], &[]);
        let Ok(cid) = cm.define_class_with_options(
            "obsolete/Probe",
            &ldc_class(70_000),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) else {
            // A manager that cannot define the probe class has nothing to
            // redefine; the pure merge tests above still cover the pool.
            return;
        };
        let stamp = crate::class_redefinition_count();
        assert!(cm.redefinition_history(cid).is_none(), "never redefined");
        cm.redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        let history = cm.redefinition_history(cid).expect("the step is recorded");
        assert!(history.last_redefinition_moved_constants());
        assert!(history.is_stale(stamp));
        assert!(!history.is_stale(crate::class_redefinition_count()));
        let moved = history
            .translate(stamp, 8)
            .expect("the old ldc operand maps");
        let cls = cm.class_store.get(cid).expect("still loaded");
        assert!(matches!(cls.constant_pool.get(8), Some(E::Integer(80_000))));
        assert!(matches!(
            cls.constant_pool.get(moved),
            Some(E::Integer(70_000))
        ));
        let map = |i: u16| history.translate(stamp, i);
        assert_eq!(
            translate_code(&[0x12, 8, 0xac], &map),
            Some(vec![0x12, u8::try_from(moved).unwrap_or(0), 0xac])
        );
    }

    /// Interpreter round i1 wave 24, lane L3: a translation is kept as runs
    /// and answers every index as the dense map it was built from did.
    #[test]
    fn translations_are_kept_as_runs() {
        // #0 and #4 name nothing; #1..#3 keep their slots; the tail #5..#9
        // moved up by two as a block; #10 moved down.
        let dense: Vec<u16> = vec![0, 1, 2, 3, 0, 7, 8, 9, 10, 11, 4];
        let translation = Translation::from_dense(&dense);
        assert_eq!(translation.runs.len(), 5, "{translation:?}");
        for (index, &merged) in dense.iter().enumerate() {
            let index = u16::try_from(index).unwrap_or(u16::MAX);
            assert_eq!(translation.get(index), Some(merged), "#{index}");
        }
        assert_eq!(translation.get(11), None, "past the old pool");
        let mut image = Vec::new();
        translation.mark_image(None, &mut image);
        let named: Vec<usize> = (0..image.len()).filter(|&i| image[i]).collect();
        assert_eq!(named, vec![1, 2, 3, 4, 7, 8, 9, 10, 11]);
        let mut keep = vec![false; dense.len()];
        keep[6] = true;
        let mut image = Vec::new();
        translation.mark_image(Some(&keep), &mut image);
        let named: Vec<usize> = (0..image.len()).filter(|&i| image[i]).collect();
        assert_eq!(named, vec![8]);
    }

    /// Interpreter round i1 wave 24, lane L3 (the i22-L3 census proposal's
    /// stage 3; the i19-L3 untranslatable-frames page, case 2): a class
    /// retransformed with a fresh constant each time, with no stale frame
    /// holding its history, used to carry every constant any version had
    /// into every later merge -- 9 + 1536 slots after 1536 retransforms of
    /// this 9-slot class, and a `u2` overflow after ~65,000 -- and its
    /// translations grew with it. Now the dead tail is left out once it
    /// reaches `COMPACT_TAIL_MIN`, and every frame the kept history can
    /// translate still reads its own constant, across each compaction.
    #[test]
    fn a_merged_pool_drops_constants_no_kept_step_reaches() {
        use crate::{ClassLoaderId, ClassManager, DefineClassOptions, RedefineOptions};
        let mut cm = ClassManager::new(&[], &[], &[]);
        let Ok(cid) = cm.define_class_with_options(
            "obsolete/Probe",
            &ldc_class(70_000),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) else {
            return;
        };
        let rounds = 3 * COMPACT_TAIL_MIN as i32;
        // (stamp, the constant a frame built then loads)
        let mut stamps: Vec<(u64, i32)> = Vec::new();
        let mut longest = 0usize;
        let mut last_len = 0usize;
        let mut compactions = 0;
        for round in 1..=rounds {
            // The census finds no stale frame: only the last eight steps stay.
            let now = crate::class_redefinition_count();
            cm.prune_redefinition_histories(|_| Some(now));
            stamps.push((crate::class_redefinition_count(), 70_000 + round - 1));
            cm.redefine_class(cid, ldc_class(70_000 + round), RedefineOptions::default())
                .expect("a same-shape redefinition succeeds");
            let len = cm.class_store.get(cid).map_or(0, |cls| cls.constant_pool.len());
            longest = longest.max(len);
            if len >= last_len {
                last_len = len;
                continue;
            }
            last_len = len;
            compactions += 1;
            // Right after a compaction: the frames the last eight steps can
            // translate read their own constants.
            let history = cm.redefinition_history(cid).expect("recorded");
            let cls = cm.class_store.get(cid).expect("loaded");
            for &(stamp, constant) in stamps.iter().rev().take(MAX_KEPT_REDEFINITIONS) {
                let merged = history.translate(stamp, 8).expect("a kept step reaches it");
                assert!(
                    matches!(cls.constant_pool.get(merged), Some(E::Integer(c)) if *c == constant),
                    "a frame of the version with {constant} reads #{merged}"
                );
            }
        }
        assert!(compactions >= 2, "{compactions} compactions");
        assert!(
            longest <= 9 + COMPACT_TAIL_MIN + MAX_KEPT_REDEFINITIONS,
            "the pool stays bounded: {longest} slots"
        );
    }

    /// Two old UTF-8 entries that differ only in their lone surrogates are
    /// different constants.
    #[test]
    fn surrogate_bearing_strings_do_not_merge_with_their_lossy_twin() {
        let mut wide = HashMap::new();
        wide.insert(3u16, Arc::from(&[0xd800u16][..]));
        let old = ConstantPool::new_with_wide(
            vec![
                E::Tombstone,
                utf8("Holder"),
                E::ClassReference { name_index: 1 },
                utf8("\u{fffd}"),
                E::StringReference { string_index: 3 },
            ],
            wide,
        );
        let new = pool(vec![
            utf8("\u{fffd}"),
            E::StringReference { string_index: 3 },
        ]);
        let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[]).expect("fits");
        let moved = merged.translation[3];
        assert_ne!(moved, 3);
        assert_eq!(
            merged.constant_pool.get_utf8_wide(moved),
            Some(&[0xd800u16][..])
        );
    }
}
