//! j27 wave 8, lane W8T1 (STATICFWD21-1, reference half;
//! `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN`, **opt-in**): may a spliced
//! body that may loop COMMIT its `putstatic` and leave every later trap to the
//! chain snapshots, instead of being refused by the planner as
//! `ir-splice-putstatic-loop`?
//!
//! # The refusal this lifts
//!
//! FG16-3 commits a spliced plain `putstatic` (`IrBuilder::splice_committed_bases`):
//! a trap that resumed at the caller's `invoke` would re-run the body and
//! commit the store twice. The planner (`lib.rs` `append_ir_inline_site`)
//! therefore refuses every body that may loop, because the builder's ROUND-11
//! fence refuses the back edge it reaches after the commit, which costs a
//! whole build. The primitive half (`ir_splice_static_replay`) admits a body
//! whose replay is harmless; a reference store (`sink = new X()`, the alloc
//! kernels) is never harmless to replay, so it stayed refused.
//!
//! # Why the chain route is sound
//!
//! Under the chain fences ([`crate::ir::ir_splice_chain_fences_enabled`],
//! default ON through the chain arm; the planner's `chain_fences` answer, off
//! for an `ACC_SYNCHRONIZED` compiling method) a trap inside a splice resumes
//! the CALLEE at its own bci through the chain snapshot (JVMS frames for every
//! scope), so nothing the body committed is re-run. The builder already
//! admits, after any committed store, an instruction whose chain snapshot it
//! recorded (`relax_fence_by_chain`, the walk-top fence in `IrBuilder::build`)
//! -- backward branches included -- and then marks the graph
//! `splice_chains_required`, so the lowering refuses the compile for any
//! spliced guard without a chain (no flat fall-back that would replay). A
//! spliced back-edge poll is a chain exit or verdict-blind, never a flat
//! re-execute. This is exactly the route a loop body with a committed
//! `putfield` into a caller-visible object already takes (the planner's
//! store-fence walk cannot model a back edge and leaves such a body to the
//! builder); this module only stops the planner from pre-refusing the
//! `putstatic` form of the same shape.
//!
//! The cost of a wrong guess is a builder refusal at the pc with no chain
//! snapshot (a whole build and the W5-2 rebuild without this splice), which is
//! why the switch is opt-in until a host A/B: no answer changes either way.
//!
//! Refused here: a `synchronized` callee (a monitor window publishes no chain:
//! `Lowerer::splice_chain_for_site`), and every compile whose planner answered
//! no chain fences.

/// The opt-in switch (token `ir-splice-putstatic-loop-chain`).
pub(crate) const SWITCH: &str = "CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN";

/// Is the reference half on? Needs FG16-3
/// ([`crate::ir::ir_splice_putstatic_enabled`]), whose rows it admits. Read
/// per use; no static.
pub(crate) fn enabled() -> bool {
    crate::ir::ir_splice_putstatic_enabled() && cratonvm_types::flags::runtime_flag_on(SWITCH)
}

/// The planner's question (`append_ir_inline_site`, before its
/// `ir-splice-putstatic-loop` refusal): may `site`, a body that may loop and
/// commits a plain `putstatic`, be left to the builder's chain fences?
/// `chain_fences` is the planner's answer for this compile (the builder's
/// `splice_chain_fences`). Prints the census line when it admits.
pub(crate) fn planner_admits_chain_loop_site(
    site: &crate::InlineSite,
    caller_pc: usize,
    chain_fences: bool,
) -> bool {
    let admitted = admits(enabled(), chain_fences, site.callee_is_synchronized);
    if admitted && crate::x64::dbg_jitc_enabled() {
        eprintln!("{}", census_line(site, caller_pc));
    }
    admitted
}

/// The one decision: the switch, the compile's chain fences, and no monitor
/// window in the body.
fn admits(on: bool, chain_fences: bool, callee_is_synchronized: bool) -> bool {
    on && chain_fences && !callee_is_synchronized
}

/// `[cratonvm-jitc] ir-splice-putstatic-loop by chain <C>.<m><desc> at caller
/// pc <p>` (`CRATONVM_DBG_JITC=1`): the body the planner handed to the chain
/// fences. A following `IrBuilder::build refused` for the same compile is the
/// body's cost (a pc without a chain snapshot).
fn census_line(site: &crate::InlineSite, caller_pc: usize) -> String {
    format!(
        "[cratonvm-jitc] ir-splice-putstatic-loop by chain {}.{}{} at caller pc {}",
        site.class_name,
        site.method_name,
        site.descriptor,
        crate::ir::inline_site_caller_pc(caller_pc),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrBuilder, IrInlineSite, IrInlineTables, Op};

    /// `static int g(int n) { for (int i = 0; i < n; i++) S = i; return 0; }`
    /// (javac shape; `putstatic` at 8, back edge at 14).
    const INT_LOOP: [u8; 19] = [
        0x03, 0x3c, // 0 iconst_0; 1 istore_1
        0x1b, 0x1a, // 2 iload_1; 3 iload_0
        0xa2, 0x00, 0x0d, // 4 if_icmpge 17
        0x1b, // 7 iload_1
        0xb3, 0x00, 0x02, // 8 putstatic S
        0x84, 0x01, 0x01, // 11 iinc 1, 1
        0xa7, 0xff, 0xf4, // 14 goto 2
        0x03, 0xac, // 17 iconst_0; 18 ireturn
    ];

    /// [`INT_LOOP`] storing `null` into a reference static: `S = null` (a
    /// reference store, which the replay route never admits).
    fn ref_loop() -> [u8; 19] {
        let mut code = INT_LOOP;
        code[7] = 0x01; // aconst_null
        code
    }

    /// The planner's view of a loop body with a putstatic of class 7 (the
    /// callee's own class is 9, so it is no `static synchronized` own write).
    fn loop_site(code: &[u8], tag: u8, synchronized: bool) -> crate::InlineSite {
        crate::InlineSite {
            callee_code: code.to_vec(),
            callee_code_len: code.len(),
            callee_max_locals: 2,
            callee_num_args: 1,
            callee_is_static: true,
            return_type: b'I',
            static_field_info: vec![(8, 7, 0, tag, false)],
            class_name: "P".to_string(),
            class_id: 9,
            method_name: "g".to_string(),
            descriptor: "(I)I".to_string(),
            callee_is_synchronized: synchronized,
            ..crate::InlineSite::default()
        }
    }

    /// `append_ir_inline_site` at caller pc 3 of a 10-byte caller, with the
    /// switches given and the replay route off.
    fn append(site: crate::InlineSite, chain_fences: bool, on: bool) -> Result<(), &'static str> {
        let vals = [
            (SWITCH, Some(if on { "1" } else { "0" })),
            (crate::ir_splice_static_replay::SWITCH, Some("0")),
        ];
        cratonvm_types::flags::with_thread_overrides(&vals, || {
            let mut combined = vec![0u8; 10];
            let mut tables = IrInlineTables::default();
            let mut owned_strings: Vec<Box<str>> = Vec::new();
            let mut owned_invoke_infos: Vec<Box<crate::JitInvokeInfo>> = Vec::new();
            let mut direct_callee_entries: Vec<(usize, u64)> = Vec::new();
            let mut budget = crate::IR_INLINE_MAX_TOTAL_BYTES;
            let mut vcalls: Vec<(usize, usize)> = Vec::new();
            let mut compact = std::collections::HashMap::new();
            let mut static_init: Vec<u32> = Vec::new();
            let mut direct_calls: Vec<(usize, (usize, bool))> = Vec::new();
            let mut checkcast_seen = false;
            let mut volatile: Vec<usize> = Vec::new();
            let mut exact_nested: Vec<(usize, u32)> = Vec::new();
            crate::append_ir_inline_site(
                site,
                3,
                &mut combined,
                &mut tables,
                &mut owned_strings,
                &mut owned_invoke_infos,
                &mut direct_callee_entries,
                &mut budget,
                &mut vcalls,
                &mut compact,
                false,
                false,
                &mut static_init,
                &mut direct_calls,
                &mut checkcast_seen,
                &mut volatile,
                &mut exact_nested,
                None,
                &|_, _, _| None,
                crate::ir::SpliceFenceRelax::NONE,
                chain_fences,
            )
        })
    }

    const REFUSED: Result<(), &str> = Err("ir-splice-putstatic-loop");

    #[test]
    fn the_decision_needs_the_switch_the_chain_fences_and_no_monitor() {
        assert!(admits(true, true, false));
        assert!(!admits(false, true, false), "switch off");
        assert!(!admits(true, false, false), "no chain fences for this compile");
        assert!(!admits(true, true, true), "a synchronized body publishes no chain");
    }

    #[test]
    fn the_switch_is_opt_in_and_needs_fg16_3() {
        fn with(vals: &[(&str, Option<&str>)]) -> bool {
            cratonvm_types::flags::with_thread_overrides(vals, enabled)
        }
        assert!(!with(&[(SWITCH, None)]), "default off");
        assert!(with(&[(SWITCH, Some("1"))]));
        assert!(!with(&[
            (SWITCH, Some("1")),
            ("CRATONVM_JIT_IR_SPLICE_PUTSTATIC", Some("0"))
        ]));
    }

    /// Fails on the old planner, which refused every such body.
    #[test]
    fn the_planner_leaves_a_reference_store_loop_to_the_chain_fences() {
        let site = || loop_site(&ref_loop(), b'L', false);
        assert_eq!(append(site(), true, false), REFUSED, "switch off: refused as before");
        assert_eq!(append(site(), false, true), REFUSED, "no chain fences: refused");
        assert_ne!(append(site(), true, true), REFUSED, "on: left to the builder");
        assert_ne!(
            append(loop_site(&INT_LOOP, b'I', false), true, true),
            REFUSED,
            "a primitive store the replay route does not take is committed the same way"
        );
        assert_eq!(
            append(loop_site(&ref_loop(), b'L', true), true, true),
            REFUSED,
            "a synchronized body keeps the refusal"
        );
    }

    /// `static int f(int n) { return g(n); }` with `body` spliced at the
    /// `invokestatic` (pc 1), class 7 static S at combined pc 13, under the
    /// chain snapshots and with the given chain-fence switch.
    fn build_loop_splice(body: &[u8], tag: u8, fences: &str) -> Option<crate::ir::Graph> {
        let mut code = vec![0x1a, 0xb8, 0x00, 0x01, 0xac]; // caller, code_len 5
        code.extend_from_slice(body);
        code.extend_from_slice(&[0, 0]);
        let site = IrInlineSite {
            base: 5,
            code_len: body.len(),
            num_args: 1,
            max_locals: 2,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.g:(I)I".to_string(),
            class_id: 9,
        };
        let vals = [
            ("CRATONVM_JIT_IR_SPLICE_FRAME_STATES", Some("1")),
            ("CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES", Some(fences)),
            (crate::ir_splice_static_replay::SWITCH, Some("0")),
        ];
        cratonvm_types::flags::with_thread_overrides(&vals, || {
            let mut b = IrBuilder::new(1, 1);
            let mut tables = IrInlineTables::default();
            tables.sites.insert(1, site);
            tables.static_field_info.insert(5 + 8, (7, 0, tag, false));
            b.apply_inline_tables(tables);
            b.graph.scope_method_key = "P.f:(I)I".to_string();
            b.build(&code, 5)
        })
    }

    /// The builder half the planner relies on (no builder change in this
    /// lane): with the chain fences the committed store's back edge resumes by
    /// chain and the graph says it relies on its chains; without them the
    /// round-11 fence refuses the build.
    #[test]
    fn the_builder_relaxes_the_back_edge_after_a_committed_store_by_chain() {
        for (body, tag) in [(ref_loop(), b'L'), (INT_LOOP, b'I')] {
            let g = build_loop_splice(&body, tag, "1").expect("the chain relaxes the back edge");
            assert!(g.splice_chains_required, "tag {}", tag as char);
            assert_eq!(
                g.nodes
                    .iter()
                    .filter(|n| matches!(n.op, Op::StoreStatic { class_id: 7, .. }))
                    .count(),
                1
            );
            assert!(
                g.safepoints.iter().any(|s| s.is_splice_state() && s.bci == 5 + 14),
                "a chain snapshot at the back edge"
            );
            assert!(
                build_loop_splice(&body, tag, "0").is_none(),
                "without the chain fences the committed store fences the back edge"
            );
        }
    }

    #[test]
    fn the_census_names_the_body_and_the_caller_pc() {
        let line = census_line(&loop_site(&ref_loop(), b'L', false), 3);
        assert_eq!(
            line,
            "[cratonvm-jitc] ir-splice-putstatic-loop by chain P.g(I)I at caller pc 3"
        );
    }
}
