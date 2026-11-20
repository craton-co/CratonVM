// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! (`docs/feature-designs/class-resolution-and-caching.md`).

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
/// (`L3W24ParkedAcrossManyRenames.java`, `parkQxC`).
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
/// (`docs/feature-designs/class-resolution-and-caching.md`).
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
                if from == 0 || !keep.map_or(true, |keep| keep.get(from).copied().unwrap_or(false))
                {
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

/// The line of `bci` in `lines` (sorted by `start_pc`, stable): HotSpot's
/// `Method::line_number_from_bci`, the rule `vm::runtime::stackwalker`
/// applies to a class's current methods -- the FIRST entry starting AT `bci`,
/// else the entry with the largest `start_pc` below it, the last of equals
/// (j27 wave 5, lane STACKHUNT: an exact pc used to answer the last of equals).
pub fn line_number_at(lines: &[LineNumberEntry], bci: usize) -> Option<u16> {
    let first_at_or_after = lines.partition_point(|e| usize::from(e.start_pc) < bci);
    match lines.get(first_at_or_after) {
        Some(e) if usize::from(e.start_pc) == bci => Some(e.line_number),
        _ => first_at_or_after
            .checked_sub(1)
            .and_then(|i| lines.get(i))
            .map(|e| e.line_number),
    }
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
mod tests;
