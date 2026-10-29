// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The type-state check of a method that uses `jsr` / `ret` (class files
//! below major 51, where those opcodes are legal).
//!
//! Interpreter round i5 wave 103, lane L7
//! (`docs/feature-designs/class-resolution-and-caching.md`).
//! Such a method used to be accepted on the structural scan alone, because
//! the inference worklist (`bytecode_verifier::verify_by_inference`) keys its
//! states by pc only: the two return addresses of a subroutine called from two
//! sites merge to `Top` at the subroutine's entry and its `ret` cannot be
//! followed. HotSpot verifies these methods with libverify (`check_code.c`),
//! which tracks the subroutine context of every instruction (its "masks": the
//! subroutines the instruction runs inside and the registers each modified)
//! and refuses an ill-typed subroutine body like any other code.
//!
//! Wave 104 (same page): this walk is now libverify's merge. States are keyed
//! by `(pc, context)`, the context being the stack of subroutine ENTRIES the
//! instruction runs inside (libverify's mask list), so a subroutine body is
//! checked once with its registers merged over every caller in the same
//! context -- a register two callers leave with different types is unusable
//! inside the subroutine ("Register N contains wrong type"), as on HotSpot.
//! Each state carries, per context level, the registers modified since that
//! subroutine was entered (`check_code.c`'s mask bitmaps; a store sets the bit
//! in every level). `jsr` pushes `ReturnAddress(entry)` (libverify's
//! `ITEM_ReturnAddress` names the subroutine, not the call site); `ret` flows
//! to the instruction after EVERY `jsr` to that subroutine that was reached in
//! the outer context, with the subroutine's registers where its mask says it
//! modified them and that `jsr`'s own registers elsewhere. A `jsr` reached
//! again re-runs the `ret`s of its subroutine, so a caller found late still
//! gets its return edge. Exception handlers are entered in the context of the
//! instruction they cover. Instructions are visited in code order, sweep after
//! sweep, as libverify's `run_dataflow` does, and every refusal is reported
//! with libverify's text (`verify_old_text`) below major 50.
//!
//! Major 50: HotSpot's split verifier refuses the method first (it has no
//! `jsr` / `ret`), fails over to libverify, and when libverify refuses too it
//! reports the SPLIT verifier's message (`Verifier::verify`); see
//! `verifier::split_refusal_of_frameless_method`.
//!
//! Wave 105 (same page): states are keyed by pc only, as libverify's
//! `instruction_data` are. The mask list travels with the state; where two
//! paths meet, the merged list keeps the subroutines common to both (in the
//! existing order) with their modified-register bits OR-ed
//! (`merge_registers`' mask intersection). So one subroutine called from
//! top-level code AND from inside another subroutine is checked once, with its
//! registers merged over both callers, as on HotSpot (wave 104 kept the two
//! contexts apart and accepted a register the callers disagree on). A `ret`
//! whose subroutine is not on the (intersected) mask list is "Illegal return
//! from subroutine". A `ret` returns with its OWN mask list cut just below the
//! returning subroutine (`new_mask_count = i`), not the jsr site's list, so
//! after a shared subroutine returns into its outer caller that caller's
//! `ret` can be orphaned, as on HotSpot (wave 105 hand-back, probe R5).
//! A method whose walk exceeds the bounds below is accepted
//! on the structural scan, as before. Verdict only: the method's type maps
//! stay the `FastPathVeto::Subroutine` entry.

use std::collections::BTreeSet;

use rustc_hash::FxHashMap;

use cratonvm_reader::instruction::Instruction;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::error::LinkageError;

use super::class::Class;
use super::verify_frame::VerificationFrame;
use super::verify_insn::{verify_instruction_with, VerifyScratch};
use super::verify_old_text as old;
use super::vtype::{ClassHierarchy, VType};

/// libverify's mask list: the subroutines an instruction runs inside,
/// outermost first, each with the registers modified since it was entered
/// (`(entry, bits)`, `bits[i]`: register `i`).
type Masks = Vec<(u16, Vec<bool>)>;

/// A state of the walk.
#[derive(Clone)]
struct State {
    frame: VerificationFrame,
    masks: Masks,
}

/// Deepest subroutine nesting walked; deeper nesting is accepted unchecked.
const MAX_DEPTH: usize = 16;

/// Type-check `method` of `class` (a method with a `jsr` / `ret`). `Ok(true)`
/// when the walk checked the whole method and it verifies; `Ok(false)` when
/// the walk gave up (its bounds, an undecodable instruction) and decided
/// nothing. Wave 105 hand-back: the caller lets a complete acceptance
/// override the structural scan's own subroutine-coverage rule.
pub(crate) fn verify_subroutine_method(
    class: &Class,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
    scratch: &mut VerifyScratch,
) -> Result<bool, LinkageError> {
    match walk(class, method, hierarchy, scratch) {
        Ok(checked) => Ok(checked),
        Err((error, pc)) => {
            // Major 50: the split verifier's message wins when both
            // verifiers refuse (wave 104).
            if class.version.major == 50 && matches!(error, LinkageError::VerifyError { .. }) {
                if let Some(split) =
                    super::verifier::split_refusal_of_frameless_method(class, method, pc)
                {
                    return Err(split);
                }
            }
            Err(error)
        }
    }
}

/// The registers `insn` writes (beyond those whose type changed, which the
/// caller adds): libverify sets a mask bit for every store, even one that
/// leaves the type as it was.
fn stored_registers(insn: &Instruction) -> (Option<u16>, bool) {
    match insn {
        Instruction::Istore(i)
        | Instruction::Fstore(i)
        | Instruction::Astore(i)
        | Instruction::Iinc { index: i, .. } => (Some(*i), false),
        Instruction::Lstore(i) | Instruction::Dstore(i) => (Some(*i), true),
        _ => (None, false),
    }
}

/// Set register `i` in every level of `masks`.
fn mark(masks: &mut Masks, i: usize) {
    for (_, level) in masks.iter_mut() {
        if let Some(bit) = level.get_mut(i) {
            *bit = true;
        }
    }
}

/// Merge the mask list `incoming` into `existing` (libverify's
/// `merge_registers`): keep the subroutines both lists hold, in `existing`'s
/// order, with their bits OR-ed. `true` when `existing` changed.
fn merge_masks(existing: &mut Masks, incoming: &Masks) -> bool {
    let before = existing.len();
    existing.retain(|(entry, _)| incoming.iter().any(|(e, _)| e == entry));
    let mut changed = existing.len() != before;
    for (entry, bits) in existing.iter_mut() {
        let Some((_, other)) = incoming.iter().find(|(e, _)| e == entry) else {
            continue;
        };
        for (b, &n) in bits.iter_mut().zip(other.iter()) {
            if n && !*b {
                *b = true;
                changed = true;
            }
        }
    }
    changed
}

/// The walk; a refusal comes with the pc it was found at.
fn walk(
    class: &Class,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
    scratch: &mut VerifyScratch,
) -> Result<bool, (LinkageError, usize)> {
    let Some(code_attr) = method.code() else {
        return Ok(true);
    };
    let bytecode: &[u8] = &code_attr.code;
    let code_len = bytecode.len();
    if code_len == 0 {
        return Ok(true);
    }
    let class_name: &str = &class.name;
    let self_named = crate::bytecode_verifier::hidden_self_named_pool(class);
    let cp = self_named.as_ref().unwrap_or(&class.constant_pool);
    // Every refusal of this walk is libverify's (`(class: ..) reason`): it
    // runs only below major 51, and a major-50 refusal is replaced by the
    // split verifier's message where that can be named (wave 104 hand-back).
    let old_texts = true;
    let control_rules = crate::loader_flags().verify_control_rules;
    let old_merges = old_texts && control_rules;
    let max_locals = usize::from(code_attr.max_locals);
    // A refusal: libverify's whole message below major 50, ours otherwise.
    let refuse = |reason: &str, pc: usize| -> (LinkageError, usize) {
        let message = if old_texts {
            old::message(class_name, &method.name, &method.descriptor, reason)
        } else {
            format!("at bytecode offset {pc}: {reason}")
        };
        (
            LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message,
            },
            pc,
        )
    };
    let max_states = code_len.saturating_mul(16).saturating_add(1024);
    let max_steps = code_len.saturating_mul(64).saturating_add(4096);

    // Every `jsr` of the method, by subroutine entry: `(jsr pc, return pc)`.
    let mut jsr_sites: FxHashMap<u16, Vec<(usize, usize)>> = FxHashMap::default();
    {
        let mut pc = 0usize;
        while pc < code_len {
            let Ok((insn, next_pc)) = Instruction::decode(bytecode, pc) else {
                // The structural scan decoded every instruction; not reached.
                return Ok(false);
            };
            if next_pc <= pc {
                return Ok(false);
            }
            let offset = match insn {
                Instruction::Jsr(o) => Some(i64::from(o)),
                Instruction::JsrW(o) => Some(i64::from(o)),
                _ => None,
            };
            if let Some(offset) = offset {
                let target = pc as i64 + offset;
                let Ok(entry) = u16::try_from(target) else {
                    return Ok(false);
                };
                jsr_sites.entry(entry).or_default().push((pc, next_pc));
            }
            pc = next_pc;
        }
    }
    // The `ret`s walked for each subroutine entry.
    let mut rets_seen: FxHashMap<u16, BTreeSet<usize>> = FxHashMap::default();
    // The `ret` that returned to each `jsr` (libverify's per-jsr record).
    let mut ret_of_jsr: FxHashMap<usize, usize> = FxHashMap::default();

    // One state per pc, as libverify's `instruction_data` (wave 105).
    let mut states: FxHashMap<usize, State> = FxHashMap::default();
    let mut pending: BTreeSet<usize> = BTreeSet::new();
    let start = 0usize;
    states.insert(
        start,
        State {
            frame: VerificationFrame::initial_frame(
                class_name,
                &method.name,
                &method.descriptor,
                method.is_static(),
                code_attr.max_locals,
                code_attr.max_stack,
            ),
            masks: Masks::new(),
        },
    );
    pending.insert(start);

    let mut sweep_from = 0usize;
    let mut steps = 0usize;
    loop {
        let next = pending
            .range(sweep_from..)
            .next()
            .copied()
            .or_else(|| pending.iter().next().copied());
        let Some(pc) = next else {
            break;
        };
        pending.remove(&pc);
        sweep_from = pc + 1;
        steps += 1;
        if steps > max_steps || states.len() > max_states {
            // Bounds: accept on the structural scan, as before this walk.
            return Ok(false);
        }
        let Some(State { frame: pre, masks: pre_masks }) = states.get(&pc).cloned() else {
            continue;
        };
        let Ok((insn, next_pc)) = Instruction::decode(bytecode, pc) else {
            return Ok(false);
        };
        let mut current = pre.clone();
        let result = match verify_instruction_with(
            &insn,
            pc,
            &mut current,
            cp,
            class_name,
            &method.name,
            &method.descriptor,
            hierarchy,
            scratch,
        ) {
            Ok(result) => result,
            Err(e) => {
                let fault = current.take_fault();
                if old_texts {
                    let reason = old::instruction_reason(&old::OldCtx {
                        insn: &insn,
                        pre: &pre,
                        fault: fault.as_ref(),
                        cp,
                        class_name,
                        method_name: &method.name,
                        descriptor: &method.descriptor,
                        is_static: method.is_static(),
                    });
                    if let Some(reason) = reason {
                        return Err(refuse(&reason, pc));
                    }
                }
                return Err(match e {
                    // No `check_code.c` reason could be named: our reason, in
                    // libverify's format.
                    LinkageError::VerifyError { message, .. } => refuse(&message, pc),
                    other => (other, pc),
                });
            }
        };
        let first_target = (&result.branch_targets).into_iter().next().copied();

        // The registers this instruction modified, in every level.
        let mut masks = pre_masks;
        if !masks.is_empty() {
            let (stored, wide) = stored_registers(&insn);
            if let Some(i) = stored {
                mark(&mut masks, usize::from(i));
                if wide {
                    mark(&mut masks, usize::from(i) + 1);
                }
            }
            for i in 0..max_locals {
                if pre.locals.get(i) != current.locals.get(i) {
                    mark(&mut masks, i);
                }
            }
        }

        let mut successors: Vec<(usize, State)> = Vec::new();
        match insn {
            Instruction::Jsr(_) | Instruction::JsrW(_) => {
                let Some(entry) = first_target else {
                    return Ok(false);
                };
                // `check_code.c`: a subroutine entered again from inside
                // itself.
                if masks.iter().any(|(e, _)| *e == entry) {
                    return Err(refuse("Recursive call to jsr entry", pc));
                }
                if masks.len() >= MAX_DEPTH {
                    return Ok(false);
                }
                // libverify's return address names the subroutine.
                if let Some(top) = current.stack.last_mut() {
                    *top = VType::ReturnAddress(entry);
                }
                let mut inner_masks = masks.clone();
                inner_masks.push((entry, vec![false; max_locals]));
                successors.push((
                    usize::from(entry),
                    State {
                        frame: current.clone(),
                        masks: inner_masks,
                    },
                ));
                // The subroutine's `ret`s already walked owe this call site
                // its return edge (and its registers may have changed).
                if let Some(rets) = rets_seen.get(&entry) {
                    for &ret_pc in rets {
                        pending.insert(ret_pc);
                    }
                }
            }
            Instruction::Ret(_) => {
                // `verify_instruction_with` answered the subroutine entry the
                // register's return address names; it must be one of this
                // state's subroutines.
                let Some(entry) = first_target else {
                    return Ok(false);
                };
                // `check_code.c` remembers which `ret` returns to each `jsr`;
                // a second `ret` returning to one is "Multiple returns to
                // single jsr" (measured on HotSpot 25 for a `ret` of a
                // subroutine that has already returned, w104 K4, where the
                // address is no longer on the mask list: that check comes
                // first).
                let sites = jsr_sites.get(&entry).map(Vec::as_slice).unwrap_or(&[]);
                if sites
                    .iter()
                    .any(|(jsr_pc, _)| ret_of_jsr.get(jsr_pc).is_some_and(|&r| r != pc))
                {
                    return Err(refuse("Multiple returns to single jsr", pc));
                }
                let Some(depth) = masks.iter().rposition(|(e, _)| *e == entry) else {
                    return Err(refuse("Illegal return from subroutine", pc));
                };
                rets_seen.entry(entry).or_default().insert(pc);
                let modified = masks[depth].1.clone();
                for &(jsr_pc, return_pc) in sites {
                    if return_pc >= code_len {
                        continue;
                    }
                    let Some(site) = states.get(&jsr_pc) else {
                        // That call site was not reached yet.
                        continue;
                    };
                    ret_of_jsr.insert(jsr_pc, pc);
                    let mut frame = current.clone();
                    {
                        let locals: &mut Vec<VType> = &mut frame.locals;
                        for (i, slot) in locals.iter_mut().enumerate() {
                            if !modified.get(i).copied().unwrap_or(false) {
                                *slot = site.frame.locals.get(i).cloned().unwrap_or(VType::Top);
                            }
                        }
                    }
                    // The RET state's own mask list cut just below the
                    // returning subroutine (`check_code.c`: `new_mask_count =
                    // i` where `masks[i].entry == called`), not the jsr
                    // site's list. Its outer levels already carry every
                    // register the subroutine modified (`mark` sets each
                    // level). Wave 105 hand-back: with the pc-keyed merge a
                    // subroutine entered from top level and from inside
                    // another has the intersected list, so the outer
                    // subroutine's `ret` after the return finds itself
                    // missing ("Illegal return from subroutine", probe R5),
                    // as on HotSpot.
                    let out_masks: Masks = masks[..depth].to_vec();
                    successors.push((
                        return_pc,
                        State {
                            frame,
                            masks: out_masks,
                        },
                    ));
                }
            }
            _ => {
                if result.falls_through {
                    if next_pc >= code_len {
                        if control_rules {
                            return Err(refuse("Falling off the end of the code", pc));
                        }
                    } else {
                        successors.push((
                            next_pc,
                            State {
                                frame: current.clone(),
                                masks: masks.clone(),
                            },
                        ));
                    }
                }
                for &target in &result.branch_targets {
                    let target = usize::from(target);
                    if target < code_len {
                        successors.push((
                            target,
                            State {
                                frame: current.clone(),
                                masks: masks.clone(),
                            },
                        ));
                    }
                }
            }
        }
        for entry in &code_attr.exception_table {
            if pc >= usize::from(entry.start_pc) && pc < usize::from(entry.end_pc) {
                let catch_type =
                    super::bytecode_verifier::catch_type_of(entry, cp, class_name, &method.name)
                        .map_err(|e| (e, pc))?;
                let mut handler = current.clone();
                handler.clear_stack();
                if handler.push(catch_type).is_err() {
                    return Ok(false);
                }
                successors.push((
                    usize::from(entry.handler_pc),
                    State {
                        frame: handler,
                        masks: masks.clone(),
                    },
                ));
            }
        }

        for (target_pc, incoming) in successors {
            let Some(existing) = states.get_mut(&target_pc) else {
                states.insert(target_pc, incoming);
                pending.insert(target_pc);
                continue;
            };
            if old_merges {
                if let Some(reason) = old::merge_reason(&existing.frame, &incoming.frame, hierarchy)
                {
                    return Err(refuse(&reason, pc));
                }
            }
            let mut changed = false;
            if !incoming.frame.is_assignable_to(&existing.frame, hierarchy) {
                let merged = existing
                    .frame
                    .merge(&incoming.frame, hierarchy)
                    .map_err(|_| refuse("Mismatched stack types", pc))?;
                if !merged.is_assignable_to(&existing.frame, hierarchy)
                    || !existing.frame.is_assignable_to(&merged, hierarchy)
                {
                    existing.frame = merged;
                    changed = true;
                }
            }
            if merge_masks(&mut existing.masks, &incoming.masks) {
                changed = true;
            }
            if changed {
                pending.insert(target_pc);
            }
        }
    }
    Ok(true)
}
