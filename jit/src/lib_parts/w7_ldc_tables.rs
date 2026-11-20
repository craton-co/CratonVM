// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The single-pass tables of a method's `ldc` / `ldc_w` / `ldc2_w` sites,
//! resolved from the constant pool before the backend runs. Split out of
//! `build_single_pass_tables` (`part_16.rs`) unchanged; the caller maps a
//! [`LdcBail`] onto its own `jitc_bail!` / `jitc_permanent_bail!`.

#[allow(unused_imports)]
use super::super::*;

/// Every row the `ldc` family contributes to the single-pass tables.
#[derive(Default)]
pub(crate) struct LdcTables {
    pub(crate) ldc_info: Vec<(usize, i64)>,
    pub(crate) ldc_string_info: Vec<(usize, u32, u16)>,
    pub(crate) ldc_class_info: Vec<(usize, u32, u16)>,
    pub(crate) ldc_slots: std::collections::HashMap<(u32, u16), usize>,
    pub(crate) ldc_fp_pcs: FxHashSet<usize>,
    pub(crate) ldc2w_info: Vec<(usize, i64)>,
}

/// Why [`resolve_ldc_tables`] declined the compile, and with which bail site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LdcBail {
    /// A resolution-timing miss (`jitc_bail!`): the compile may be retried.
    Transient(&'static str),
    /// A property of the class file (`jitc_permanent_bail!`).
    Permanent(&'static str),
}

/// Resolve the `ldc` / `ldc_w` (`ldc_ops`) and `ldc2_w` (`ldc2w_ops`) sites.
pub(crate) fn resolve_ldc_tables(
    ldc_ops: &[(usize, u16)],
    ldc2w_ops: &[(usize, u16)],
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<JitLdcConstant>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<(i64, bool)>>,
    helpers: &crate::JitRuntimeHelpers,
) -> Result<LdcTables, LdcBail> {
    // Resolve ldc/ldc_w constants (int/float from CP). A `None` from the
    // resolver means the constant is not representable as an immediate
    // (String/Class/MethodHandle ldc) — bail the whole compile, mirroring
    // the ldc2_w arm below. The previous `unwrap_or(0)` would have compiled
    // `ldc "str"` as pushing constant 0 (a null reference) — wrong code.
    // With no resolver at all, `ldc_info` stays empty and the 0x12/0x13
    // codegen arm bails per-site instead.
    let mut ldc_info: Vec<(usize, i64)> = Vec::new();
    let mut ldc_string_info: Vec<(usize, u32, u16)> = Vec::new();
    let mut ldc_class_info: Vec<(usize, u32, u16)> = Vec::new();
    // Round 9 wave 7 (ldc7 request A.3): `(holder, cp) -> GC-maintained slot`
    // for String/Class sites the VM reported as `*Slot`. Keyed by site, not
    // pc, so the bytecode loop rewriter needs no lifting for it. Empty (the
    // kill switch `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0`, or no resolved
    // slot) = every site keeps its helper CALL.
    let mut ldc_slots: std::collections::HashMap<(u32, u16), usize> =
        std::collections::HashMap::new();
    // The `is_float`/`is_double` halves the codegen throws away. The deopt
    // operand-stack snapshot has no consuming opcode to ask for the width, so
    // it needs the constant-pool tag itself — see `Compiler::ldc_fp_pcs`.
    let mut ldc_fp_pcs: FxHashSet<usize> = FxHashSet::default();
    if !ldc_ops.is_empty() {
        if let Some(resolver) = cp_ldc_resolver {
            for &(pc, cp_idx) in ldc_ops {
                let resolved = resolver(cp_idx);
                if let Some(
                    JitLdcConstant::StringSlot {
                        holder_class_id,
                        cp_idx,
                        slot_addr,
                    }
                    | JitLdcConstant::ClassMirrorSlot {
                        holder_class_id,
                        cp_idx,
                        slot_addr,
                    }
                    | JitLdcConstant::DynamicSlot {
                        holder_class_id,
                        cp_idx,
                        slot_addr,
                    },
                ) = &resolved
                {
                    if *slot_addr != 0 {
                        ldc_slots.insert((*holder_class_id, *cp_idx), *slot_addr);
                    }
                }
                match resolved {
                    // The single-pass backend ignores `is_float`: it pushes the
                    // bits and the consuming opcode picks the width. See the
                    // variant's doc for why the IR builder cannot.
                    Some(JitLdcConstant::Immediate { bits, is_float }) => {
                        ldc_info.push((pc, bits));
                        if is_float {
                            ldc_fp_pcs.insert(pc);
                        }
                    }
                    // JIT r23 wave 4b, lane L2 (`CRATONVM_JIT_LDC_CONDY_SLOT`):
                    // a resolved reference condy is emitted as a slotted
                    // CP site (`emit_ldc_cp_site`): its probe of the
                    // never-cleared slot is the whole instruction. The helper
                    // arm behind it is unreachable (a published slot word is
                    // never zero) and, if it ran, `jit_ldc_class_cp`'s
                    // recorded-hit arm answers the same recorded condy result
                    // before any class resolution. Without a slot the site is
                    // declined, NOT permanently: the next offer mints it.
                    Some(JitLdcConstant::DynamicSlot {
                        holder_class_id,
                        cp_idx,
                        slot_addr,
                    }) => {
                        if helpers.ldc_class_cp == 0 || slot_addr == 0 {
                            return Err(LdcBail::Transient("ldc-condy-slot-unwired"));
                        }
                        ldc_class_info.push((pc, holder_class_id, cp_idx));
                    }
                    Some(JitLdcConstant::Dynamic { .. }) => {
                        return Err(LdcBail::Transient("ldc-condy-unslotted"));
                    }
                    Some(JitLdcConstant::ClassMirror {
                        holder_class_id,
                        cp_idx,
                    })
                    | Some(JitLdcConstant::ClassMirrorSlot {
                        holder_class_id,
                        cp_idx,
                        ..
                    }) => {
                        // The helper is `OptionalPtr`: a hand-built test table
                        // leaves it 0, and then this site keeps the historical
                        // whole-compile bail rather than emitting a CALL to 0.
                        if helpers.ldc_class_cp == 0 {
                            return Err(LdcBail::Transient("ldc_class_helper_unwired"));
                        }
                        ldc_class_info.push((pc, holder_class_id, cp_idx));
                    }
                    Some(JitLdcConstant::String {
                        holder_class_id,
                        cp_idx,
                    })
                    | Some(JitLdcConstant::StringSlot {
                        holder_class_id,
                        cp_idx,
                        ..
                    }) => {
                        // Same OptionalPtr guard the `ClassMirror` arm above
                        // takes: a hand-built test table leaves the helper 0,
                        // and then this site keeps the historical whole-compile
                        // bail rather than emitting a CALL to address 0.
                        if helpers.ldc_string_cp == 0 {
                            return Err(LdcBail::Transient("ldc_string_helper_unwired"));
                        }
                        ldc_string_info.push((pc, holder_class_id, cp_idx));
                    }
                    None => {
                        // RBC.7 — same permanent-bail class as RBC.4 (scan
                        // reject) / RBC.6 (athrow+handler): this resolver's
                        // `None` means the constant pool entry at `cp_idx` is
                        // a String/Class/MethodHandle (not representable as
                        // an immediate) — a property of the class file that
                        // never changes, not a resolution-timing miss.
                        // Without marking it, a hot method containing
                        // `ldc "str"` re-ran the whole upgrade gauntlet
                        // (skip-list + native-shadow walks + this scan) every
                        // JIT_RETRY_STRIDE calls forever (same pathology RBC.4
                        // fixed for scan rejects — observed as a silent,
                        // diagnostic-free hang: TestResponsePerformance's
                        // trivial `getRequestURI() { return "..."; }` bailed
                        // on every one of ~1M hot-loop calls).
                        return Err(LdcBail::Permanent("ldc-constant-unsupported"));
                    }
                }
            }
        }
    }

    // Resolve ldc2_w constants (long/double from CP). Single-pass types the
    // value by the consuming opcode (lstore/dstore/…), so it ignores the inc-35
    // `is_double` flag and keeps just the bits.
    let mut ldc2w_info: Vec<(usize, i64)> = Vec::new();
    if !ldc2w_ops.is_empty() {
        let Some(resolver) = cp_ldc2w_resolver else {
            return Err(LdcBail::Transient("cp_ldc2w_resolver"));
        };
        for &(pc, cp_idx) in ldc2w_ops {
            let (val, is_double) = match resolver(cp_idx) {
                Some(v) => v,
                None => {
                    // RBC.7 twin: a non-Long/Double constant at this ldc2_w
                    // index is likewise fixed by the bytecode — permanent
                    // bail, not a transient miss. See the ldc arm above.
                    return Err(LdcBail::Permanent("ldc2w-constant-unsupported"));
                }
            };
            ldc2w_info.push((pc, val));
            if is_double {
                ldc_fp_pcs.insert(pc);
            }
        }
    }
    Ok(LdcTables {
        ldc_info,
        ldc_string_info,
        ldc_class_info,
        ldc_slots,
        ldc_fp_pcs,
        ldc2w_info,
    })
}
