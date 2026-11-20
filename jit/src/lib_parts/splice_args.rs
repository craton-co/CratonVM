// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Planning helpers for `Op::IndyRootRef` array metadata, write-once
//! `static final` / `invokedynamic` call-site receiver classes, and 100%-covered
//! bimorphic trap-miss sites.

use crate::bytecode_analysis as ba;
use crate::deopt::{speculation_id, DeoptReason, DespecRegistry};
use crate::ir::{self, Graph, IrBuilder, Op};
use crate::profile::{MethodProfile, ReceiverEvidence};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Populate `const_ref_meta.array_length` (and matching `trusted_static_array_lengths` /
/// `trusted_final_static_fields` entries) for `Op::IndyRootRef` slots emitted from
/// initialized non-volatile `static final` array fields.
pub(crate) fn feed_trusted_static_root_ref_arrays(
    graph: &mut Graph,
    static_base_known: impl Fn(u32, usize) -> bool,
    length_of: Option<&dyn Fn(u32, u32) -> Option<i32>>,
) {
    let mut keys: Vec<(usize, (u32, u32))> = Vec::new();
    for (&slot_addr, meta) in &graph.const_ref_meta {
        if let Some(key) = meta.static_field {
            if static_base_known(key.0, key.1 as usize) {
                keys.push((slot_addr, key));
            }
        }
    }
    for (slot_addr, key) in keys {
        if !graph.trusted_final_static_fields.contains(&key) {
            graph.trusted_final_static_fields.push(key);
        }
        if let Some(resolver) = length_of {
            if let Some(len) = resolver(key.0, key.1).filter(|&l| l >= 0) {
                if let Some(meta) = graph.const_ref_meta.get_mut(&slot_addr) {
                    meta.array_length = Some(len);
                    meta.non_null = true;
                }
                if !graph
                    .trusted_static_array_lengths
                    .iter()
                    .any(|&(k, _)| k == key)
                {
                    graph.trusted_static_array_lengths.push((key, len));
                }
            }
        }
    }
    // Also propagate lengths already present on `trusted_static_array_lengths` into `const_ref_meta`.
    for meta in graph.const_ref_meta.values_mut() {
        if meta.array_length.is_none() {
            if let Some(key) = meta.static_field {
                if let Some(&(_, len)) = graph
                    .trusted_static_array_lengths
                    .iter()
                    .find(|&&(k, _)| k == key)
                {
                    if len >= 0 {
                        meta.array_length = Some(len);
                        meta.non_null = true;
                    }
                }
            }
        }
    }
}

/// Resolve an exact receiver class for a write-once local `k` assigned directly
/// from an initialized `static final` field (`0xb2` getstatic) or constant-class
/// `invokedynamic` (`0xba`) prior to `call_pc`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_once_static_or_indy_local_class(
    builder: &IrBuilder,
    static_final_ref_class: Option<&dyn Fn(u32, u32) -> Option<u32>>,
    indy_classes: &HashMap<usize, u32>,
    code: &[u8],
    code_len: usize,
    handler_pcs: &[usize],
    call_pc: usize,
    local_k: usize,
) -> Option<u32> {
    let len = code_len.min(code.len());
    let insns = ba::decode_method(code, len)?;
    let mut targets: HashSet<usize> = handler_pcs.iter().copied().collect();
    let mut scratch = Vec::new();
    for insn in &insns {
        scratch.clear();
        if !ba::explicit_targets(code, len, insn.pc, &mut scratch) {
            return None;
        }
        targets.extend(scratch.iter().copied());
    }
    let slot_of = |insn: &ba::Insn| -> Option<(usize, bool)> {
        let index = |insn: &ba::Insn| -> Option<usize> {
            Some(if insn.wide {
                usize::from(u16::from_be_bytes([
                    *code.get(insn.pc + 2)?,
                    *code.get(insn.pc + 3)?,
                ]))
            } else {
                usize::from(*code.get(insn.pc + 1)?)
            })
        };
        match insn.op {
            0x36..=0x3a | 0x84 => Some((index(insn)?, matches!(insn.op, 0x37 | 0x39))),
            0x3b..=0x4e => Some((
                usize::from((insn.op - 0x3b) % 4),
                matches!((insn.op - 0x3b) / 4, 1 | 3),
            )),
            _ => None,
        }
    };
    let mut store_insn_idx: Option<usize> = None;
    for (i, insn) in insns.iter().enumerate() {
        if let Some((slot, wide)) = slot_of(insn) {
            if slot == local_k {
                if store_insn_idx.is_some() || !matches!(insn.op, 0x3a | 0x4b..=0x4e) {
                    return None;
                }
                store_insn_idx = Some(i);
            } else if wide && slot + 1 == local_k {
                return None;
            }
        }
    }
    let store_idx = store_insn_idx?;
    if store_idx == 0 {
        return None;
    }
    let store_insn = &insns[store_idx];
    if store_insn.pc >= call_pc || targets.contains(&store_insn.pc) {
        return None;
    }
    // Must not lie behind any branch target ambiguity before the call if skipped:
    // require no explicit branch jumping over the store into a block reaching `call_pc`
    // trivially when `store_insn.pc == insns[store_idx - 1].pc + ...`, and `prev` is straight-line.
    let prev = &insns[store_idx - 1];
    match prev.op {
        0xb2 => {
            let (class_id, index, b'L', false) = builder.static_field_row(prev.pc)? else {
                return None;
            };
            let idx = u32::try_from(index).ok()?;
            static_final_ref_class?(class_id, idx)
                .filter(|&c| c != 0 && c != ir::STATIC_FINAL_OBJECT_FILED)
        }
        0xba => indy_classes.get(&prev.pc).copied().filter(|&c| c != 0),
        _ => None,
    }
}

/// Register bimorphic and polymorphic guarded receiver sites on `builder`, and
/// mark 100%-covered exact two-class sites as trap-miss guarded
/// (`add_guarded_trap_site`) so C2 sees a call-free body while still deopting
/// soundly if an unseen class arrives.
pub(crate) fn reg_receiver_sites(
    builder: &mut IrBuilder,
    bimorphic_sites: &[(usize, u32)],
    polymorphic_sites: &mut Vec<(usize, Vec<u32>)>,
    profile: Option<&MethodProfile>,
    despec: Option<&Arc<DespecRegistry>>,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) {
    for &(pc, second_class) in bimorphic_sites {
        builder.add_bimorphic_receiver_site(pc, second_class);
        if !ir::ir_guarded_splice_trap_miss_enabled()
            || polymorphic_sites.iter().any(|&(p, _)| p == pc)
            || second_class == 0
        {
            continue;
        }
        let Some(counts) = profile.and_then(|p| p.receivers.get(&pc)) else {
            continue;
        };
        let evidence = ReceiverEvidence::of(counts);
        let summary = evidence.summary();
        let covered = summary.observations >= crate::MIN_GUARDED_RECEIVER_EVIDENCE
            && summary.types == 2
            && summary.is_exact()
            && summary
                .top
                .is_some_and(|(c, _)| c != 0 && c != second_class)
            && summary.second.is_some_and(|(c, _)| c == second_class);
        if !covered {
            continue;
        }
        let bci = pc as u32;
        let withdrawn = despec.is_some_and(|registry| {
            registry.contains_speculation(
                &format!("{class_name}.{method_name}:{method_descriptor}"),
                bci,
                speculation_id(bci, DeoptReason::UncommonTrap),
            )
        });
        if !withdrawn {
            builder.add_guarded_trap_site(pc);
        }
    }
    for (pc, extra) in std::mem::take(polymorphic_sites) {
        builder.add_polymorphic_receiver_sites(pc, extra);
    }
}

/// Populate both legacy `LoadStatic` and `Op::IndyRootRef` trusted array
/// lengths on `graph` from `length_of`.
pub(crate) fn feed_trusted_arrays(
    graph: &mut Graph,
    static_base_known: impl Fn(u32, usize) -> bool,
    length_of: Option<&dyn Fn(u32, u32) -> Option<i32>>,
) {
    if crate::ir_optimize::ir_static_final_array_length_enabled() {
        if let Some(len_fn) = length_of {
            super::ir_ffm_element_call_admits::ir_feed_trusted_static_arrays(graph, &static_base_known, len_fn);
        }
        feed_trusted_static_root_ref_arrays(graph, &static_base_known, length_of);
    } else {
        feed_trusted_static_root_ref_arrays(graph, &static_base_known, None);
    }
}

/// Is the `Integer.valueOf(I)` / `Long.valueOf(J)` call at `pc` javac's
/// boxing of a reflective call's varargs element: its box stored straight
/// into the array (`aastore` next) and the next call `Method.invoke`?
pub(crate) fn reflective_varargs_box_site(
    code: &[u8],
    code_len: usize,
    pc: usize,
    (class, name, desc): (&str, &str, &str),
    next_invoke: Option<(&str, &str)>,
) -> bool {
    matches!(
        (class, name, desc),
        ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;")
            | ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;")
    ) && pc + 3 < code_len
        && code.get(pc + 3) == Some(&0x53)
        && next_invoke == Some(("java/lang/reflect/Method", "invoke"))
}

/// Stamp splice metadata + method identity and emit post-compile IR telemetry.
#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_ir_compiled_telemetry(
    compiled: &mut crate::CompiledMethod,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    ir_spliced_bodies_pure: bool,
    ir_guarded_splice_bcis: &[u32],
    ir_guarded_splice_classes: &[(u32, u32)],
    ir_splice_scope_class_ids: &[(String, u32)],
    ir_splice_scope_sources: &[super::jit_code_region::SpliceScopeSource],
    chain_census: Option<(std::time::Instant, ir::ChainSnapshotCensus, usize)>,
    ir_emit_long: bool,
    code: &[u8],
    code_len: usize,
    metrics: &crate::metrics::CompileRecorder,
) {
    let method_key = format!("{class_name}.{method_name}:{method_descriptor}");
    compiled.stamp_deopt_method_key(&method_key);
    compiled.spliced_bodies_side_effect_free = ir_spliced_bodies_pure;
    if !ir_guarded_splice_bcis.is_empty() {
        let mut bcis = ir_guarded_splice_bcis.to_vec();
        bcis.sort_unstable();
        bcis.dedup();
        compiled.ir_guarded_splice_bcis = bcis.into_boxed_slice();
    }
    if !ir_guarded_splice_classes.is_empty() {
        let mut rows = ir_guarded_splice_classes.to_vec();
        rows.sort_by_key(|&(bci, _)| bci);
        rows.dedup_by_key(|&mut (bci, _)| bci);
        compiled.ir_guarded_splice_classes = rows.into_boxed_slice();
    }
    compiled.splice_scope_class_ids = ir_splice_scope_class_ids.to_vec();
    compiled.splice_scope_sources = ir_splice_scope_sources.to_vec();
    compiled.method_label = method_key;
    compiled.used_ir_backend = true;
    if !compiled.ir_prim_colour_claim.is_empty() && crate::x64::dbg_jitc_enabled() {
        let (before, now) = compiled.ir_prim_colour_clear_bytes;
        eprintln!(
            "[cratonvm-jitc] ir-prim-colour-claim: {} frame {} bytes, clear {} -> {} \
             bytes, {} prim word(s) claimed",
            compiled.method_label,
            compiled.osr_frame_size,
            before,
            now,
            compiled.ir_prim_colour_claim.len(),
        );
    }
    if !compiled.ir_frame_anatomy.is_empty() && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] ir-frame-anatomy: {} {}",
            compiled.method_label, compiled.ir_frame_anatomy,
        );
    }
    if let Ok(want) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        let full = format!("{class_name}.{method_name}{method_descriptor}");
        if full.contains(&want) {
            let slice = compiled._buffer_slice_for_debug();
            let mut hex = String::new();
            for b in slice {
                hex.push_str(&format!("{:02x}", b));
            }
            eprintln!(
                "[JIT_CODE] backend=ir {} entry={:p} len={}\n{}",
                full,
                compiled.entry,
                slice.len(),
                hex
            );
        }
    }
    if let Some((t0, census, kept)) = chain_census {
        if census.recorded > 0 || census.skipped > 0 {
            let published = compiled
                ._deopt_point_boxes
                .iter()
                .filter(|p| p.frame_state.caller.is_some())
                .count();
            eprintln!(
                "[ir] chain-snapshots {class_name}.{method_name}{method_descriptor}: \
                 lazy={} recorded={} skipped={} recorded_words={} kept={} published={} \
                 code={} compile_us={}",
                u8::from(ir::ir_chain_snapshots_lazy_enabled()),
                census.recorded,
                census.skipped,
                census.recorded_words,
                kept,
                published,
                compiled._buffer_slice_for_debug().len(),
                t0.elapsed().as_micros(),
            );
        }
    }
    #[cfg(test)]
    crate::IR_LOWER_COMPILES.with(|c| c.set(c.get() + 1));
    let _ = (ir_emit_long, code, code_len);
    metrics.installed(compiled);
    if crate::dbg_intrinsic_enabled() {
        eprintln!(
            "[cratonvm-intrinsic] IR body installed (single-pass call-site intrinsics NOT registered) {class_name}.{method_name}{method_descriptor}",
        );
    }
}

/// Emit stage-reporting diagnostics when IR lowering or acceptance declines.
pub(super) fn report_ir_tier_decline(
    acceptance_discarded: bool,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) {
    if crate::ir_stage_reporting() && acceptance_discarded {
        eprintln!(
            "[ir] optimizing body lowered, then discarded by the acceptance gate \
             (not a lower_inner refusal) for {class_name}.{method_name}{method_descriptor}",
        );
    } else if crate::ir_stage_reporting() {
        eprintln!(
            "[ir] ir_lower::lower_inner refused ({}) for {class_name}.{method_name}{method_descriptor}",
            crate::ir_lower::last_lower_bail().unwrap_or("unlabelled"),
        );
    }
}
