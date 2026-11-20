// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Sparse `lookupswitch` binary-search lowering and large-dispatcher splice admission.

use super::*;

/// Minimum number of live (non-default) sorted cases before a sparse switch
/// emits a balanced `Cmp(Lt)` / `Cmp(Eq)` search tree instead of a linear chain.
pub(in crate::ir) const SPARSE_SWITCH_MIN_CASES: usize = 6;

const DISPATCHER_MIN_INVOKES: usize = 16;
const DISPATCHER_LARGE_LOOKUPSWITCH_BYTES: usize = 150;
const DISPATCHER_MAX_SPLICE_SUBTREE_BYTES: usize = 200;

/// Returns the strictly ascending live `(key, target_pc)` pairs when a sparse
/// switch should be lowered as a balanced search tree, or `None` to keep the
/// linear equality chain.
pub(in crate::ir) fn sparse_switch_cases(
    cases: &[(i32, usize)],
    default_target: usize,
) -> Option<Vec<(i32, usize)>> {
    if !cases.windows(2).all(|w| w[0].0 < w[1].0) {
        return None;
    }
    let live: Vec<(i32, usize)> = cases
        .iter()
        .copied()
        .filter(|&(_, target)| target != default_target)
        .collect();
    if live.len() < SPARSE_SWITCH_MIN_CASES {
        return None;
    }
    Some(live)
}

/// Returns `true` when the root method is a large switch/multi-invoke
/// dispatcher (`>= 16` call sites or a `lookupswitch` exceeding 150 bytes).
pub(in crate::ir) fn is_large_switch_or_multi_invoke_root(
    code: &[u8],
    code_len: usize,
    verified: &cratonvm_reader::VerifiedCode,
) -> bool {
    let mut invoke_count = 0usize;
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len {
            break;
        }
        let op = code[pc];
        if matches!(op, 0xb6..=0xb9) {
            invoke_count += 1;
            if invoke_count >= DISPATCHER_MIN_INVOKES {
                return true;
            }
        } else if op == 0xab
            && (insn.next_pc as usize).saturating_sub(pc) > DISPATCHER_LARGE_LOOKUPSWITCH_BYTES
        {
            return true;
        }
    }
    false
}

fn body_has_loop(code: &[u8], base: usize, body_len: usize) -> bool {
    let Some(body) = code.get(base..base.saturating_add(body_len)) else {
        return false;
    };
    cratonvm_reader::verified_code(body).is_ok_and(|v| !v.loop_headers().is_empty())
}

/// Filters loop-bearing or oversized splice trees out of large dispatcher
/// roots (leaving small leaf intrinsics spliced and keeping `Op::Call` fallback
/// via `invoke_info`), then publishes the build's splice resume map.
pub(in crate::ir) fn prepare_build_inline_sites(
    builder: &mut IrBuilder,
    code: &[u8],
    code_len: usize,
    verified: &cratonvm_reader::VerifiedCode,
) {
    if !builder.inline_sites.is_empty()
        && is_large_switch_or_multi_invoke_root(code, code_len, verified)
    {
        filter_dispatcher_loop_splices(builder, code, code_len);
    }
    set_splice_resume_map_this_build(if builder.inline_sites.is_empty() {
        None
    } else {
        Some(IrSpliceResumeMap::from_sites(
            code_len,
            &builder.inline_sites,
        ))
    });
}

fn filter_dispatcher_loop_splices(builder: &mut IrBuilder, code: &[u8], code_len: usize) {
    let mut top_by_pc: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut nested_keys: Vec<(usize, usize)> = Vec::new();
    for &key in builder.inline_sites.keys() {
        let caller_pc = inline_site_caller_pc(key);
        if caller_pc < code_len {
            top_by_pc.entry(caller_pc).or_default().push(key);
        } else {
            nested_keys.push((caller_pc, key));
        }
    }

    let mut drop_keys: HashSet<usize> = HashSet::new();
    let mut drop_pcs: HashSet<usize> = HashSet::new();
    for (root_pc, root_keys) in top_by_pc {
        // Keep sites without an `invoke_info` row so fallback to `Op::Call` is guaranteed.
        if !builder.invoke_info.contains_key(&root_pc) {
            continue;
        }
        let mut work = root_keys;
        let mut visited_keys: HashSet<usize> = HashSet::new();
        let mut subtree_pcs: Vec<usize> = vec![root_pc];
        let mut subtree_bytes = 0usize;
        let mut has_loop = false;
        while let Some(key) = work.pop() {
            if !visited_keys.insert(key) {
                continue;
            }
            let Some(site) = builder.inline_sites.get(&key) else {
                continue;
            };
            subtree_bytes = subtree_bytes.saturating_add(site.code_len);
            if body_has_loop(code, site.base, site.code_len) {
                has_loop = true;
            }
            let start = site.base;
            let end = site.base.saturating_add(site.code_len);
            for &(npc, nkey) in &nested_keys {
                if (start..end).contains(&npc) && !visited_keys.contains(&nkey) {
                    subtree_pcs.push(npc);
                    work.push(nkey);
                }
            }
        }
        if has_loop || subtree_bytes > DISPATCHER_MAX_SPLICE_SUBTREE_BYTES {
            drop_keys.extend(visited_keys);
            drop_pcs.extend(subtree_pcs);
        }
    }

    if drop_keys.is_empty() {
        return;
    }
    for key in drop_keys {
        builder.inline_sites.remove(&key);
    }
    for pc in drop_pcs {
        builder.exact_receiver_sites.remove(&pc);
        builder.guarded_receiver_sites.remove(&pc);
        builder.cha_watermark_sites.remove(&pc);
        builder.cha_class_set_sites.remove(&pc);
        builder.guarded_hot_cap_sites.remove(&pc);
        builder.nested_guarded_sites.remove(&pc);
        builder.bimorphic_sites.remove(&pc);
        builder.polymorphic_sites.remove(&pc);
        builder.guarded_trap_sites.remove(&pc);
        builder.sync_splice_sites.remove(&pc);
        builder.sync_splice_mirrors.remove(&pc);
        builder.sync_splice_held_sites.remove(&pc);
    }
}
