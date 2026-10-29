//! j27 wave 7, lane T1STATICFWD (STATICFWD21-1, primitive half;
//! `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY`, **opt-in**): may a spliced
//! body that may loop carry a plain primitive `putstatic` WITHOUT raising the
//! builder's replay fence?
//!
//! # Why the fence exists
//!
//! Every trap inside a splice (a null check, a bounds check, a division by
//! zero, a deopt at a poll) resumes at the CALLER's `invoke` and re-runs the
//! whole call. FG16-3 therefore COMMITS a spliced `putstatic`
//! (`IrBuilder::splice_committed_bases`), after which only instructions that
//! cannot trap may follow; a backward branch never may, so the planner refuses
//! every body that may loop (`ir-splice-putstatic-loop` in `lib.rs`'s
//! `append_ir_inline_site`), and every alloc / sum kernel ending in a static
//! write keeps its call.
//!
//! # When a replay is harmless
//!
//! A replay re-runs the body from its first instruction with the same
//! arguments. It computes and stores the same values when nothing the body
//! READS can have been changed by what the body WROTE before the trap:
//!
//! * the body's only memory writes are plain non-volatile PRIMITIVE
//!   `putstatic`s of a class the resolver found initialised (so the write runs
//!   no `<clinit>`): no `putfield`, array store, allocation, call,
//!   `invokedynamic`, monitor or `athrow`;
//! * it never reads a static it writes (`S = S + 1` is not idempotent), and
//!   reads other statics only of a class it also writes (initialised, so the
//!   read runs no `<clinit>` that could change state the body read earlier);
//! * no access is volatile (no acquire / release to repeat), and the callee is
//!   not `synchronized` (the planner checks `callee_is_synchronized`, the
//!   builder the frame's monitor window).
//!
//! Reference stores are NOT idempotent (a replay stores a different
//! allocation, which a racing reader can tell apart), so they stay committed;
//! their route is the chain-snapshot half of STATICFWD21-1
//! (`crate::ir_splice_static_chain`, j27 wave 8, opt-in).
//!
//! What a replay CAN show is a racing reader seeing a cell go back to an
//! earlier value of the same run (`S = 5`, trap, replay `S = 0`). JLS 17.4
//! gives plain (non-volatile) accesses no per-variable coherence, so that is
//! an allowed outcome; it is also the reason this stays opt-in until a host
//! A/B and the JMM probe rows say otherwise.
//!
//! **Racy inputs** (j27 wave 9, lane W9RPL): a replay RE-READS heap memory
//! another thread may have changed since the first run (`S = a[i]` in a loop:
//! the first run published `a0[0]`, the replay publishes `a1[0]`; no single
//! run wrote both). So a body that reads a field, an array element or a
//! static must not be able to trap once one of its stores has run
//! ([`no_trap_follows_a_store`]): then a replay only discards a run that
//! published nothing. `sumInto` (every read before its one store) keeps the
//! route; `fillFrom` (store, back edge, read) refuses and keeps its call.
//!
//! # Depth
//!
//! Only the OUTERMOST splice: a body nested in another may be re-run by the
//! ENCLOSING body's trap after the enclosing body read what this one wrote.
//! The builder asks for depth 1 itself; the planner admits only a body whose
//! caller pc lies in the compiling method's own code
//! ([`caller_pc_is_top_level`]), so it never admits a body the builder will
//! then commit (a whole-method refusal and a W5-2 rebuild).
//!
//! One decision, two askers: the planner (`append_ir_inline_site`) and the
//! builder (`IrBuilder::splice_static_store_replays_idempotently`) both call
//! [`body_replays_idempotently`] over the same bytes and rows.

use std::collections::HashMap;

use crate::bytecode_analysis;
use crate::ir::IrInlineSite;

/// The opt-in switch (token `ir-splice-putstatic-loop-replay`).
pub(crate) const SWITCH: &str = "CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY";

/// `(class id, field index, type tag, volatile)` of one static row, the shape
/// both `IrInlineTables::static_field_info` and the builder's table carry.
pub(crate) type StaticRow = (u32, usize, u8, bool);

/// Is STATICFWD21-1's primitive half on? Needs FG16-3
/// ([`crate::ir::ir_splice_putstatic_enabled`]), whose rows it reads. Read per
/// use; no static.
pub(crate) fn enabled() -> bool {
    crate::ir::ir_splice_putstatic_enabled() && cratonvm_types::flags::runtime_flag_on(SWITCH)
}

/// Does re-running `body` (a callee's bytecode, body-relative pcs) from its
/// first instruction store the same values into the same statics? See the
/// module comment. `static_row(pc)` answers the row of the `getstatic` /
/// `putstatic` at `pc`; `getfield_is_volatile(pc)` whether the `getfield` at
/// `pc` reads a volatile field. `false` for a body with no `putstatic`, and
/// for anything it cannot decode.
pub(crate) fn body_replays_idempotently(
    body: &[u8],
    static_row: impl Fn(usize) -> Option<StaticRow>,
    getfield_is_volatile: impl Fn(usize) -> bool,
) -> bool {
    let Some(written) = written_statics(body, &static_row) else {
        return false;
    };
    !written.is_empty()
        && every_insn_replays(body, &written, &static_row, &getfield_is_volatile)
        && (!reads_shared_memory(body) || no_trap_follows_a_store(body))
}

/// j27 wave 9 (lane W9RPL; page
/// `j27-splice-putstatic-loop-replay-racy-inputs-FIXED-20261029.md`): does `body`
/// read memory another thread may write (a `getfield`, an array load, a
/// `getstatic`)? A replay re-reads it, so after a published store it could
/// publish a value computed from contents no single run read. Assumes
/// `body` decodes ([`written_statics`] checked it).
fn reads_shared_memory(body: &[u8]) -> bool {
    insn_pcs(body).any(|pc| matches!(body[pc], 0x2e..=0x35 | 0xb2 | 0xb4))
}

/// Can no trap (and so no replay) happen once a `putstatic` of `body` has
/// run? Every instruction reachable after one must be in
/// [`cannot_trap_after_store`]'s set and branch only forward (a backward edge
/// carries a poll). Then a replay only ever discards a run that published
/// nothing, and the racy reads it repeats are the only reads of the run.
fn no_trap_follows_a_store(body: &[u8]) -> bool {
    let len = body.len();
    let mut seen = vec![false; len];
    let mut work: Vec<usize> = insn_pcs(body)
        .filter(|&pc| body[pc] == 0xb3)
        .flat_map(|pc| bytecode_analysis::lenient_successors(body, len, pc))
        .collect();
    while let Some(pc) = work.pop() {
        if pc >= len || std::mem::replace(&mut seen[pc], true) {
            continue;
        }
        let succ = bytecode_analysis::lenient_successors(body, len, pc);
        if !cannot_trap_after_store(body, pc) || succ.iter().any(|&s| s <= pc) {
            return false;
        }
        work.extend(succ);
    }
    true
}

/// The instructions the builder builds without a trap inside an outermost
/// splice whose classes are initialised: the committed-splice fence's set
/// (`IrBuilder::admits_after_committed_splice_store`) minus everything this
/// route already refuses. No `ldc` (its slot may be unresolved), no div /
/// rem, no switch.
fn cannot_trap_after_store(body: &[u8], pc: usize) -> bool {
    match body[pc] {
        0x00..=0x11 | 0x15..=0x2d | 0x36..=0x4e | 0x57..=0x5f => true,
        0x60..=0x6b | 0x74..=0x98 => true,
        // if*, goto, ifnull, ifnonnull, goto_w (direction checked by the caller)
        0x99..=0xa7 | 0xc6..=0xc8 => true,
        // returns; get/putstatic of an initialised class (`written_statics`,
        // `getstatic_replays` vetted the rows)
        0xac..=0xb1 | 0xb2 | 0xb3 => true,
        0xc4 => body
            .get(pc + 1)
            .is_some_and(|&w| matches!(w, 0x15..=0x19 | 0x36..=0x3a | 0x84)),
        _ => false,
    }
}

/// The instruction start pcs of `body`, stopping at the first that does not
/// decode.
fn insn_pcs(body: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut pc = 0usize;
    std::iter::from_fn(move || {
        let here = pc;
        let len = bytecode_analysis::insn_len(body, here).filter(|&l| l > 0)?;
        pc = here + len;
        Some(here)
    })
    .take_while(move |&p| p < body.len())
}

/// The planner's question (`append_ir_inline_site`, before its
/// `ir-splice-putstatic-loop` refusal): is `site`, about to be appended for
/// the call at `caller_pc`, an outermost, non-`synchronized` body whose
/// replay is harmless? The builder asks the same of the same bytes
/// (`IrBuilder::splice_static_store_replays_idempotently`).
pub(crate) fn planner_admits_loop_site(
    site: &crate::InlineSite,
    sites: &HashMap<usize, IrInlineSite>,
    caller_pc: usize,
) -> bool {
    let len = site.callee_code_len.min(site.callee_code.len());
    enabled()
        && !site.callee_is_synchronized
        && caller_pc_is_top_level(sites, caller_pc)
        && body_replays_idempotently(
            &site.callee_code[..len],
            |pc| {
                site.static_field_info
                    .iter()
                    .find(|r| r.0 == pc)
                    .map(|&(_, class_id, field_index, tag, is_volatile)| {
                        (class_id, field_index, tag, is_volatile)
                    })
            },
            |pc| site.volatile_field_pcs.contains(&pc),
        )
}

/// May the call at `caller_pc` (a site-table key) be the OUTERMOST splice,
/// i.e. does it lie in no body already appended to `sites`? A nested body's
/// parent is inserted into the table before its nested rows are appended.
pub(crate) fn caller_pc_is_top_level(sites: &HashMap<usize, IrInlineSite>, caller_pc: usize) -> bool {
    let pc = crate::ir::inline_site_caller_pc(caller_pc);
    !sites
        .values()
        .any(|s| s.base <= pc && pc < s.base.saturating_add(s.code_len))
}

/// Primitive field types (JVMS 4.3.2 base types).
fn is_primitive_tag(tag: u8) -> bool {
    matches!(tag, b'I' | b'Z' | b'B' | b'C' | b'S' | b'J' | b'D' | b'F')
}

/// The `(class, field)` of every `putstatic` in `body`, or `None` when one is
/// not a plain primitive write of a resolved class, or the bytes do not
/// decode.
fn written_statics(
    body: &[u8],
    static_row: &impl Fn(usize) -> Option<StaticRow>,
) -> Option<Vec<(u32, usize)>> {
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < body.len() {
        let len = bytecode_analysis::insn_len(body, pc).filter(|&l| l > 0)?;
        if body.get(pc) == Some(&0xb3) {
            let (class_id, field_index, tag, is_volatile) = static_row(pc)?;
            if class_id == 0 || is_volatile || !is_primitive_tag(tag) {
                return None;
            }
            out.push((class_id, field_index));
        }
        pc += len;
    }
    Some(out)
}

/// Is every instruction of `body` one a replay re-runs harmlessly, given the
/// statics it writes (`written`)?
fn every_insn_replays(
    body: &[u8],
    written: &[(u32, usize)],
    static_row: &impl Fn(usize) -> Option<StaticRow>,
    getfield_is_volatile: &impl Fn(usize) -> bool,
) -> bool {
    let mut pc = 0usize;
    while pc < body.len() {
        let Some(len) = bytecode_analysis::insn_len(body, pc).filter(|&l| l > 0) else {
            return false;
        };
        let admitted = match body[pc] {
            0xb2 => getstatic_replays(static_row(pc), written),
            0xb4 => !getfield_is_volatile(pc),
            // wide: a local load / store or `iinc`, never `wide ret`.
            0xc4 => body
                .get(pc + 1)
                .is_some_and(|&w| matches!(w, 0x15..=0x19 | 0x36..=0x3a | 0x84)),
            op => opcode_replays(op),
        };
        if !admitted {
            return false;
        }
        pc += len;
    }
    true
}

/// A plain read of a static the body does not write, of a class it writes.
fn getstatic_replays(row: Option<StaticRow>, written: &[(u32, usize)]) -> bool {
    let Some((class_id, field_index, _, is_volatile)) = row else {
        return false;
    };
    !is_volatile
        && class_id != 0
        && written.iter().any(|&(c, _)| c == class_id)
        && !written.contains(&(class_id, field_index))
}

/// The opcodes that write no memory, take no lock and call nothing. A trap
/// some of them can raise (array loads, `idiv`, `arraylength`) is fine: the
/// replay re-runs it and raises it again.
fn opcode_replays(op: u8) -> bool {
    matches!(
        op,
        // nop, constants, bipush, sipush, ldc / ldc_w / ldc2_w (the resolver
        // admits only immediate, String and Class constants in a splice)
        0x00..=0x14
        // local loads, array loads
        | 0x15..=0x35
        // local stores
        | 0x36..=0x4e
        // pop .. swap
        | 0x57..=0x5f
        // arithmetic, iinc, conversions, compares
        | 0x60..=0x98
        // if*, if_icmp*, if_acmp*, goto (backward too: the point of this)
        | 0x99..=0xa7
        // tableswitch, lookupswitch
        | 0xaa
        | 0xab
        // returns
        | 0xac..=0xb1
        // putstatic: vetted by `written_statics`
        | 0xb3
        // arraylength
        | 0xbe
        // ifnull, ifnonnull, goto_w
        | 0xc6..=0xc8
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrBuilder, IrInlineTables, Op};

    const S: StaticRow = (7, 0, b'I', false);

    fn rows(list: &[(usize, StaticRow)]) -> impl Fn(usize) -> Option<StaticRow> + '_ {
        move |pc| list.iter().find(|r| r.0 == pc).map(|r| r.1)
    }

    fn no_volatile(_: usize) -> bool {
        false
    }

    /// `static int g(int n) { for (int i = 0; i < n; i++) S = i; return 0; }`
    /// (javac shape; `putstatic` at 8).
    const BLIND_LOOP: [u8; 19] = [
        0x03, 0x3c, // 0 iconst_0; 1 istore_1
        0x1b, 0x1a, // 2 iload_1; 3 iload_0
        0xa2, 0x00, 0x0d, // 4 if_icmpge 17
        0x1b, // 7 iload_1
        0xb3, 0x00, 0x02, // 8 putstatic S
        0x84, 0x01, 0x01, // 11 iinc 1, 1
        0xa7, 0xff, 0xf4, // 14 goto 2
        0x03, 0xac, // 17 iconst_0; 18 ireturn
    ];

    #[test]
    fn a_loop_storing_only_a_local_into_a_primitive_static_replays() {
        assert!(body_replays_idempotently(&BLIND_LOOP, rows(&[(8, S)]), no_volatile));
    }

    #[test]
    fn a_body_reading_the_static_it_writes_does_not() {
        // `S = S + 1`: 0 getstatic S; 3 iconst_1; 4 iadd; 5 putstatic S;
        // 8 return
        let body = [0xb2, 0x00, 0x02, 0x04, 0x60, 0xb3, 0x00, 0x02, 0xb1];
        assert!(!body_replays_idempotently(&body, rows(&[(0, S), (5, S)]), no_volatile));
        // Another field of the same (initialised) class may be read...
        let other_field = (7, 1, b'I', false);
        assert!(body_replays_idempotently(
            &body,
            rows(&[(0, other_field), (5, S)]),
            no_volatile
        ));
        // ...but not one of a class it does not write (its `<clinit>`).
        let other_class = (8, 0, b'I', false);
        assert!(!body_replays_idempotently(
            &body,
            rows(&[(0, other_class), (5, S)]),
            no_volatile
        ));
        // Nor a volatile one.
        let volatile = (7, 1, b'I', true);
        assert!(!body_replays_idempotently(&body, rows(&[(0, volatile), (5, S)]), no_volatile));
    }

    #[test]
    fn a_reference_volatile_rowless_or_classless_store_does_not() {
        for row in [
            (7, 0, b'L', false),
            (7, 0, b'[', false),
            (7, 0, b'I', true),
            (0, 0, b'I', false),
        ] {
            assert!(
                !body_replays_idempotently(&BLIND_LOOP, rows(&[(8, row)]), no_volatile),
                "{row:?}"
            );
        }
        assert!(!body_replays_idempotently(&BLIND_LOOP, rows(&[]), no_volatile));
    }

    #[test]
    fn every_other_write_call_monitor_or_throw_refuses() {
        // Replace the `iinc` at 11 (3 bytes) by each 3-byte-or-shorter
        // offender, padded with `nop`s so the branch offsets stay valid.
        for offender in [
            &[0x4f, 0x00, 0x00][..], // iastore
            &[0xb5, 0x00, 0x03],     // putfield
            &[0xb8, 0x00, 0x04],     // invokestatic
            &[0xb6, 0x00, 0x04],     // invokevirtual
            &[0xbb, 0x00, 0x05],     // new
            &[0xbc, 0x0a, 0x00],     // newarray
            &[0xc2, 0x00, 0x00],     // monitorenter
            &[0xbf, 0x00, 0x00],     // athrow
            &[0xc0, 0x00, 0x06],     // checkcast
        ] {
            let mut body = BLIND_LOOP;
            body[11..14].copy_from_slice(offender);
            assert!(
                !body_replays_idempotently(&body, rows(&[(8, S)]), no_volatile),
                "{offender:02x?}"
            );
        }
    }

    #[test]
    fn a_volatile_getfield_refuses_and_a_plain_one_replays() {
        // 0 aload_0; 1 getfield #3; 4 putstatic S; 7 return
        let body = [0x2a, 0xb4, 0x00, 0x03, 0xb3, 0x00, 0x02, 0xb1];
        assert!(body_replays_idempotently(&body, rows(&[(4, S)]), no_volatile));
        assert!(!body_replays_idempotently(&body, rows(&[(4, S)]), |pc| pc == 1));
    }

    #[test]
    fn wide_local_forms_replay_and_wide_ret_does_not() {
        // 0 wide iload 1; 4 putstatic S; 7 wide iinc 1 1; 13 return
        let body = [
            0xc4, 0x15, 0x00, 0x01, 0xb3, 0x00, 0x02, 0xc4, 0x84, 0x00, 0x01, 0x00, 0x01, 0xb1,
        ];
        assert!(body_replays_idempotently(&body, rows(&[(4, S)]), no_volatile));
        let mut ret = body;
        ret[1] = 0xa9; // wide ret 1
        assert!(!body_replays_idempotently(&ret, rows(&[(4, S)]), no_volatile));
    }

    /// `static void fillFrom(int[] a, int n) { for (int i = 0; i < n; i++) S = a[i]; }`
    /// (`putstatic` at 10): a store, then a trap point (the back edge's poll,
    /// the next `iaload`), then a fresh read of the shared array.
    const FILL_FROM: [u8; 20] = [
        0x03, 0x3d, // 0 iconst_0; 1 istore_2
        0x1c, 0x1b, // 2 iload_2; 3 iload_1
        0xa2, 0x00, 0x0f, // 4 if_icmpge 19
        0x2a, 0x1c, 0x2e, // 7 aload_0; 8 iload_2; 9 iaload
        0xb3, 0x00, 0x02, // 10 putstatic S
        0x84, 0x02, 0x01, // 13 iinc 2, 1
        0xa7, 0xff, 0xf2, // 16 goto 2
        0xb1, // 19 return
    ];

    /// `static void sumInto(int[] a) { int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; S = s; }`
    /// (`putstatic` at 23): every shared read precedes the only store.
    const SUM_INTO: [u8; 27] = [
        0x03, 0x3c, 0x03, 0x3d, // 0 iconst_0; istore_1; iconst_0; istore_2
        0x1c, 0x2a, 0xbe, // 4 iload_2; 5 aload_0; 6 arraylength
        0xa2, 0x00, 0x0f, // 7 if_icmpge 22
        0x1b, 0x2a, 0x1c, 0x2e, 0x60, 0x3c, // 10 iload_1 .. 15 istore_1
        0x84, 0x02, 0x01, // 16 iinc 2, 1
        0xa7, 0xff, 0xf1, // 19 goto 4
        0x1b, // 22 iload_1
        0xb3, 0x00, 0x02, // 23 putstatic S
        0xb1, // 26 return
    ];

    /// j27 wave 9 (lane W9RPL): another thread may rewrite `a` between the
    /// first run and the replay, so a replay after a published store could
    /// publish a value no single run wrote. A store that a trap point can
    /// follow refuses once the body reads shared memory.
    #[test]
    fn a_shared_read_with_a_trap_after_a_store_refuses() {
        assert!(!body_replays_idempotently(&FILL_FROM, rows(&[(10, S)]), no_volatile));
        // The same store loop over locals only keeps replaying.
        assert!(body_replays_idempotently(&BLIND_LOOP, rows(&[(8, S)]), no_volatile));
    }

    #[test]
    fn shared_reads_all_before_the_only_store_replay() {
        assert!(body_replays_idempotently(&SUM_INTO, rows(&[(23, S)]), no_volatile));
    }

    #[test]
    fn a_trapping_division_after_the_store_refuses_only_with_a_shared_read() {
        // 0 aload_0; 1 getfield #3; 4 putstatic S; 7 iload_1; 8 iconst_1;
        // 9 idiv; 10 pop; 11 return
        let body = [0x2a, 0xb4, 0x00, 0x03, 0xb3, 0x00, 0x02, 0x1b, 0x04, 0x6c, 0x57, 0xb1];
        assert!(!body_replays_idempotently(&body, rows(&[(4, S)]), no_volatile));
        // 0 iload_0; 1 nop; 2 nop; 3 nop; then the same tail: no shared read.
        let mut local = body;
        local[..4].copy_from_slice(&[0x1a, 0x00, 0x00, 0x00]);
        assert!(body_replays_idempotently(&local, rows(&[(4, S)]), no_volatile));
        // A getstatic of another field of the written class is shared too.
        // 0 getstatic T; 3 nop; then the same tail.
        let mut stat = body;
        stat[..4].copy_from_slice(&[0xb2, 0x00, 0x04, 0x00]);
        let t = (7, 1, b'I', false);
        assert!(!body_replays_idempotently(&stat, rows(&[(0, t), (4, S)]), no_volatile));
    }

    #[test]
    fn a_body_with_no_store_is_not_this_route() {
        // 0 iconst_0; 1 ireturn
        assert!(!body_replays_idempotently(&[0x03, 0xac], rows(&[]), no_volatile));
    }

    fn site(base: usize, code_len: usize) -> IrInlineSite {
        IrInlineSite {
            base,
            code_len,
            num_args: 1,
            max_locals: 2,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.g:(I)I".to_string(),
            class_id: 7,
        }
    }

    #[test]
    fn only_a_caller_pc_outside_every_appended_body_is_top_level() {
        let sites = HashMap::from([(1usize, site(5, 19))]);
        assert!(caller_pc_is_top_level(&sites, 3), "the method's own code");
        assert!(!caller_pc_is_top_level(&sites, 5 + 7), "inside the body");
        assert!(caller_pc_is_top_level(&sites, 24), "past it");
        assert!(
            !caller_pc_is_top_level(&sites, crate::ir::bimorphic_second_site_key(9)),
            "a tagged key is read as its caller pc"
        );
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

    /// `static int f(int n) { return g(n); }` with [`BLIND_LOOP`] spliced at
    /// the `invokestatic` (pc 1), class 7 static S at combined pc 13.
    fn build_blind_loop_splice(switch_on: bool) -> Option<crate::ir::Graph> {
        let mut code = vec![0x1a, 0xb8, 0x00, 0x01, 0xac]; // caller, code_len 5
        code.extend_from_slice(&BLIND_LOOP);
        code.extend_from_slice(&[0, 0]);
        cratonvm_types::flags::with_thread_overrides(
            &[(SWITCH, Some(if switch_on { "1" } else { "0" }))],
            || {
                let mut b = IrBuilder::new(1, 1);
                let mut tables = IrInlineTables::default();
                tables.sites.insert(1, site(5, BLIND_LOOP.len()));
                tables.static_field_info.insert(5 + 8, S);
                b.apply_inline_tables(tables);
                b.build(&code, 5)
            },
        )
    }

    #[test]
    fn the_builder_splices_the_blind_store_loop_only_with_the_switch() {
        let g = build_blind_loop_splice(true)
            .expect("an idempotent store loop builds without the replay fence");
        assert_eq!(
            g.nodes
                .iter()
                .filter(|n| matches!(n.op, Op::StoreStatic { class_id: 7, .. }))
                .count(),
            1
        );
        assert!(
            !g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
            "the call is gone"
        );
        assert!(
            build_blind_loop_splice(false).is_none(),
            "off: the committed store fences the back edge (the planner refuses \
             this body as `ir-splice-putstatic-loop`)"
        );
    }
}
