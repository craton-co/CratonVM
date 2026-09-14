// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JEP 358 for an NPE raised inside COMPILED code.
//!
//! # The defect this closes
//!
//! Cold, this VM prints HotSpot's message; hot — the same call, same shape,
//! after the method is compiled — it printed nothing at all:
//!
//! ```text
//!                    HotSpot                            CratonVM (before)
//! readField cold     …because "h" is null                …because "h" is null
//! readField hot      …because "h" is null                null
//! lenOf     hot      Cannot read the array length        Cannot read the
//!                    because "a" is null                 array length
//! ```
//!
//! Two different losses, one cause. The compiled null check signals through
//! `JIT_PENDING_NPE`, and the interpreter's drain built the message from the
//! *action code* alone (`helpful_npe::jit_action_message`) — a fixed string per
//! opcode family, with no `because "…" is null` clause, and no string at all
//! for the shapes whose helper records no action (`getfield`, `putfield`,
//! `invoke*` on a null receiver, which is every object-receiver dereference).
//!
//! See `docs/internal/fixed-bugs/the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.md`.
//!
//! # Why the bytecode is the right source, and not the action code
//!
//! The action code was a *summary* of the trapping opcode, recorded at compile
//! time by whichever stub happened to know its own kind. The trapping BYTECODE
//! is the opcode itself. Given the trapping method and bci, this module reads
//! `code[bci]` and answers the same question the interpreter answers at the
//! same opcode, through the same [`helpful_npe`] analysis — so a hot row equals
//! its cold row by construction rather than by a table that has to be kept in
//! sync with the emitter.
//!
//! [`helpful_npe`]: crate::runtime::exceptions::helpful_npe
//!
//! # Where the trapping (method, bci) comes from
//!
//! `jit::helpers::snapshot_trap_frames` already captures the compiled frames
//! that were live when the helper signalled, *innermost last*, each carrying
//! the artifact's `"class/Name.method:descriptor"` label, its bci, and the
//! chain of callees the JIT inlined at that program point (innermost first).
//! That snapshot exists to give the NPE a stack trace; it names the trapping
//! program point exactly, so it answers this question too and no new plumbing
//! is needed to ask it.
//!
//! The snapshot is the ONLY source consulted. When it names no plausible bci —
//! a frame that published no safepoint id reports `-1` — this module declines
//! and the caller falls back to the action-only message, which is what the
//! drain built before. Declining is not a formality: `line_number_for_bci_in_method`
//! and the operand-stack simulation both answer confidently for an out-of-range
//! bci, so a guessed bci yields a *plausible wrong message*, which is worse
//! than none.

use std::sync::Arc;

use super::invoke::{helpful_npe_opcode_message_parts, CpPoolResolver};
use crate::jit::conservative_roots::ActiveCompiledFrame;
use crate::runtime::exceptions::helpful_npe::{self, ArrayElemKind, CpRef, CpResolver};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

/// JVMS 4.9.1 — `Code.code_length` is below 65536, so every genuine bci is.
/// Mirrors `conservative_roots::plausible_bci` and `stackwalker`'s own bound.
const MAX_CODE_LENGTH: u32 = 65_536;

/// `CRATONVM_DBG_JITNPE=1` — say, on stderr, what this module decided for each
/// JIT-originated NPE: the trapping site it recovered, and the message it
/// built or why it declined.
///
/// A refusal here is invisible in the product — the NPE still has the old
/// action-only message, or none — so "it did not engage" and "it engaged and
/// agreed with the old answer" are otherwise indistinguishable, which is the
/// same blind spot that let the whole JIT-NPE message apparatus sit wired to
/// nothing until 2026-09-06.
fn dbg() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITNPE").is_some())
}

/// The message for a JIT-originated NPE, honouring
/// `-XX:±ShowCodeDetailsInExceptionMessages` exactly as
/// [`helpful_npe::jit_npe_message_gated`] does.
///
/// Reconstructs the full JEP 358 message from the trapping compiled frame's own
/// bytecode; falls back to the action-only string when the snapshot cannot name
/// a trapping site, so this is never worse than the drain's previous answer.
///
/// `snapshot` is the trap-frame snapshot drained beside the pending-NPE flag.
/// It MUST be the snapshot for *this* NPE — a stale one names a different
/// program point and would produce a confidently wrong message.
pub(crate) fn jit_npe_message(
    shared: &SharedVm,
    snapshot: Option<&[ActiveCompiledFrame]>,
    action_code: u8,
) -> Option<String> {
    // The suppression gate first, for the reason `jit_npe_message_gated` checks
    // it first: an explicit `-XX:-ShowCodeDetailsInExceptionMessages` means
    // "HotSpot's no-message behaviour", which outranks having an answer.
    if crate::runtime::env_cache::helpful_npe_suppressed() {
        return None;
    }
    if !crate::runtime::env_cache::helpful_npe_opcodes() {
        return None;
    }
    let rebuilt = snapshot
        .and_then(innermost_trap_site)
        .and_then(|site| message_at(shared, &site, action_code));
    if dbg() {
        match &rebuilt {
            Some(m) => eprintln!("[JITNPE] rebuilt from bytecode: {m}"),
            None => eprintln!(
                "[JITNPE] declined (frames={}, innermost={:?}); falling back to action={action_code}",
                snapshot.map_or(0, <[ActiveCompiledFrame]>::len),
                snapshot.and_then(innermost_trap_site),
            ),
        }
    }
    // The fallback is `jit_npe_message_gated` and not `jit_action_message`, so
    // that the answer this function declines to improve on is produced by the
    // function that has always produced it — gates included. The two gate
    // checks above are then a second ask of the same questions, which is worth
    // one pair of `OnceLock` reads on a cold path to keep ONE owner of the
    // action-to-message mapping.
    rebuilt.or_else(|| helpful_npe::jit_npe_message_gated(action_code))
}

/// One trapping program point: the method that dereferenced null, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrapSite {
    class_name: String,
    method_name: String,
    method_descriptor: String,
    bci: usize,
    /// Did the bci come from an emitter-described NPE trap site? See
    /// [`ActiveCompiledFrame::trap_site_exact`] and [`site_is_trustworthy`].
    exact: bool,
}

/// The innermost program point in a trap-frame snapshot.
///
/// The snapshot is OUTERMOST-first (`append_snapshotted_compiled_frames` states
/// the same invariant and relies on it), so the trapping artifact is the last
/// element. Within one artifact the inline chain is INNERMOST-first, so a
/// spliced callee — which is where the null was actually dereferenced when the
/// JIT inlined the accessor — is `inline_chain[0]`.
///
/// # What makes the answer EXACT, and what only makes it usable
///
/// * **An emitter-described trap site** (`trap_site_exact`) or a chain keyed by
///   the exact return address (`chain_exact`): the bci AND the chain describe
///   the program point that trapped, so the innermost level is the trapping
///   method at its own bci. Exact, and [`site_is_trustworthy`] needs nothing
///   further.
///
/// * **Anything else**: the bci came from the frame's safepoint-id slot, and a
///   chain, if there is one, from the coarse `safepoint_bci` key. Both are what
///   the trace already PRINTS for this frame, and the emitter poisons a bci
///   whose rows disagree (`compiled_frame_inline_chain`'s `None` arm), so this
///   is not a guess — but it is not a proof either, and a message asserts more
///   than a frame does. Usable only with the corroboration
///   [`site_is_trustworthy`] demands.
fn innermost_trap_site(snapshot: &[ActiveCompiledFrame]) -> Option<TrapSite> {
    let frame = snapshot.last()?;
    let spliced = !frame.inline_chain.is_empty();
    let (label, bci) = match frame.inline_chain.first() {
        Some(level) => (level.label.as_str(), level.bci),
        // `ActiveCompiledFrame::bci` is `-1` for a frame that published no
        // usable safepoint id. That is a refusal, not a bci.
        None => (frame.label.as_str(), u32::try_from(frame.bci).ok()?),
    };
    if bci >= MAX_CODE_LENGTH {
        return None;
    }
    let (owner_and_method, descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() || descriptor.is_empty() {
        return None;
    }
    Some(TrapSite {
        class_name: class_name.to_owned(),
        method_name: method_name.to_owned(),
        method_descriptor: descriptor.to_owned(),
        // Widening: bounded above by `MAX_CODE_LENGTH`.
        bci: bci as usize,
        // A chain the emitter recorded at the trap site, or one keyed by the
        // exact return address, names the trapping method as precisely as a
        // trap key does.
        exact: frame.trap_site_exact || (spliced && frame.chain_exact),
    })
}

/// The trapping method's bytecode and the `ClassId` its constant pool belongs
/// to, or `None` when the class or the method cannot be resolved.
///
/// The bytes are COPIED rather than borrowed: every consumer below re-enters
/// the class manager (`CpPoolResolver` takes `read_recursive()` per query), and
/// handing the analysis a slice borrowed out of the same lock would tie its
/// lifetime to a guard this function is trying to drop. A trapping method's
/// `Code` is bounded by 64 KiB and this runs once per thrown NPE.
fn trapping_code(shared: &SharedVm, site: &TrapSite) -> Option<(ClassId, Arc<[u8]>)> {
    let cm = shared.classes.class_manager.read_recursive();
    let class_id = crate::runtime::stackwalker::find_class_id_by_name_memoized(
        &cm.class_store,
        &site.class_name,
    )?;
    let class = cm.class_store.get(class_id)?;
    let method = class.find_method(&site.method_name, &site.method_descriptor)?;
    let code = method.code()?;
    let bytes: Arc<[u8]> = Arc::from(&*code.code);
    Some((class_id, bytes))
}

/// Build the JEP 358 message for the opcode at `site`, or `None` when the
/// opcode there is not one that dereferences a null operand.
///
/// A `None` here is a real signal and not a formality: it means the snapshot
/// pointed at a bci whose opcode cannot raise this NPE, i.e. the site is wrong.
/// Emitting the action-only fallback in that case is exactly right — it is the
/// answer that does not depend on the site being correct.
fn message_at(shared: &SharedVm, site: &TrapSite, action_code: u8) -> Option<String> {
    let (class_id, code) = trapping_code(shared, site)?;
    let opcode = *code.get(site.bci)?;
    if !site_is_trustworthy(site, opcode, action_code) {
        return None;
    }
    let resolver = CpPoolResolver {
        shared,
        class_id,
        method_name: &site.method_name,
        method_descriptor: &site.method_descriptor,
    };
    let cp_index = || -> Option<u16> {
        Some(u16::from_be_bytes([
            *code.get(site.bci + 1)?,
            *code.get(site.bci + 2)?,
        ]))
    };

    // `invoke*` is the one shape whose operand depth is not a constant: the
    // receiver sits below its arguments, so the descriptor decides. It also
    // builds its action from the METHOD ref rather than a field name, which is
    // why it is not folded into the table below.
    if matches!(opcode, 0xb6 | 0xb7 | 0xb9) {
        let CpRef::Method {
            owner_internal,
            name,
            descriptor,
        } = resolver.method_ref(cp_index()?)?
        else {
            return None;
        };
        let action = helpful_npe::action_invoke(&owner_internal, &name, &descriptor);
        let num_params = super::count_method_params(&descriptor);
        let expr =
            helpful_npe::null_expr_for_invoke_receiver(&code, site.bci, num_params, &resolver);
        return Some(helpful_npe::combine(&action, expr.as_ref()));
    }

    // Everything else: an action half plus the operand's depth below the top of
    // the operand stack as it stood before the opcode. Both halves are the
    // interpreter's — see the matching call sites in `opcodes.rs`, which pass
    // the same depths (getfield 0, putfield 1, `*aload` 1, `*astore` 2,
    // arraylength 0, monitor 0, athrow 0).
    let (action, depth) = match opcode {
        // getfield / putfield
        0xb4 | 0xb5 => {
            let CpRef::Field { name, .. } = resolver.field_ref(cp_index()?)? else {
                return None;
            };
            if opcode == 0xb4 {
                (helpful_npe::action_read_field(&name), 0)
            } else {
                (helpful_npe::action_assign_field(&name), 1)
            }
        }
        // arraylength
        0xbe => (helpful_npe::action_array_length(), 0),
        // iaload laload faload daload aaload baload caload saload
        0x2e..=0x35 => (
            helpful_npe::action_array_load(array_elem_kind(opcode - 0x2e)),
            1,
        ),
        // iastore lastore fastore dastore aastore bastore castore sastore
        0x4f..=0x56 => (
            helpful_npe::action_array_store(array_elem_kind(opcode - 0x4f)),
            2,
        ),
        // monitorenter / monitorexit
        0xc2 | 0xc3 => (helpful_npe::action_monitor(), 0),
        // athrow
        0xbf => (helpful_npe::action_throw(), 0),
        _ => return None,
    };
    Some(helpful_npe_opcode_message_parts(
        shared,
        class_id,
        &code,
        &site.method_name,
        &site.method_descriptor,
        site.bci,
        &action,
        depth,
    ))
}

/// May the opcode at `site` be read as the one that trapped?
///
/// Two independent warrants, and one of them has to hold. Neither is a
/// formality: this function is the whole difference between a JEP 358 message
/// and a *fluent sentence about the wrong dereference*, which a reader has no
/// way to tell from a right one.
///
/// * **The emitter described this exact site.** `record_npe_trap_site` records
///   a bci AT the null check it describes, so it names the trapping program
///   point and cannot be one safepoint behind. This is the only warrant
///   available to a site whose action code is `NONE` — the object-receiver
///   shapes (`getfield`, `putfield`, `invoke*`), which is why those sites are
///   given keys rather than left to the safepoint-id slot.
///
/// * **The recorded action agrees with the opcode.** When the null-check stub
///   recorded a kind (`ARRAY_LENGTH`, `ALOAD_INT`, …), that kind is independent
///   evidence about what trapped. A bci recovered from the safepoint-id slot
///   that lands on an opcode of exactly the recorded kind is corroborated by
///   two sources that were written at different times by different code.
///
/// Failing both, the caller keeps the action-only message — precisely the
/// answer that does not depend on the bci being right.
fn site_is_trustworthy(site: &TrapSite, opcode: u8, action_code: u8) -> bool {
    site.exact || action_admits_opcode(action_code, opcode)
}

/// Does JEP-358 action `code` describe the opcode `opcode`?
///
/// `false` for [`npe_action::NONE`], which describes nothing: "no kind was
/// recorded" is not evidence that the bci is right.
///
/// [`npe_action::NONE`]: cratonvm_jit_api::npe_action::NONE
fn action_admits_opcode(code: u8, opcode: u8) -> bool {
    use cratonvm_jit_api::npe_action as a;
    // `iaload`..`saload` is 0x2e..0x35 and `iastore`..`sastore` is 0x4f..0x56,
    // both in the JVMS order `i l f d a b c s` — the same order
    // [`array_elem_kind`] decodes, so one table would serve both if the action
    // codes were in that order too. They are not (they were assigned as the
    // shapes were implemented), so this is written out.
    // The one code that names a FAMILY rather than a single opcode: JVMS gives
    // `invokevirtual` / `invokespecial` / `invokeinterface` the same receiver
    // rule, and the dispatch helper that records it serves all three.
    if code == a::INVOKE_RECEIVER {
        return matches!(opcode, 0xb6 | 0xb7 | 0xb9);
    }
    let want: u8 = match code {
        a::ARRAY_LENGTH => 0xbe,
        a::ALOAD_INT => 0x2e,
        a::ALOAD_LONG => 0x2f,
        a::ALOAD_FLOAT => 0x30,
        a::ALOAD_DOUBLE => 0x31,
        a::ALOAD_OBJECT => 0x32,
        a::ALOAD_BYTE => 0x33,
        a::ALOAD_CHAR => 0x34,
        a::ALOAD_SHORT => 0x35,
        a::ASTORE_INT => 0x4f,
        a::ASTORE_LONG => 0x50,
        a::ASTORE_FLOAT => 0x51,
        a::ASTORE_DOUBLE => 0x52,
        a::ASTORE_OBJECT => 0x53,
        a::ASTORE_BYTE => 0x54,
        a::ASTORE_CHAR => 0x55,
        a::ASTORE_SHORT => 0x56,
        _ => return false,
    };
    want == opcode
}

/// The element-type spelling for the `n`th opcode of an array load or store
/// family. Both families are laid out in the same order by JVMS §6
/// (`i l f d a b c s`), which is what lets one table serve both.
///
/// `baload`/`bastore` cover `byte[]` AND `boolean[]`; the array's own element
/// type is what separates them, and this path does not have the array — it is
/// null. HotSpot has the same problem and resolves it the same way when the
/// type is unavailable, so `byte` is the spelling here. It is the one cell in
/// this module that can differ from the interpreter's answer, and only when the
/// trapping array is a `boolean[]`.
fn array_elem_kind(n: u8) -> ArrayElemKind {
    match n {
        0 => ArrayElemKind::Int,
        1 => ArrayElemKind::Long,
        2 => ArrayElemKind::Float,
        3 => ArrayElemKind::Double,
        4 => ArrayElemKind::Object,
        5 => ArrayElemKind::Byte,
        6 => ArrayElemKind::Char,
        _ => ArrayElemKind::Short,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::conservative_roots::InlinedLevel;

    fn frame(label: &str, bci: i32, chain: Vec<InlinedLevel>) -> ActiveCompiledFrame {
        ActiveCompiledFrame {
            interp_depth: 0,
            label: label.to_string(),
            owner_class_id: 0,
            cm_ptr: 0,
            bci,
            inline_chain: chain,
            chain_exact: true,
            trap_site_exact: false,
        }
    }

    /// The same frame with a chain recovered from the COARSE safepoint-bci key.
    fn frame_coarse_chain(label: &str, bci: i32, chain: Vec<InlinedLevel>) -> ActiveCompiledFrame {
        ActiveCompiledFrame {
            chain_exact: false,
            ..frame(label, bci, chain)
        }
    }

    fn level(label: &str, bci: u32) -> InlinedLevel {
        InlinedLevel {
            label: label.to_string(),
            bci,
            class_id: 0,
        }
    }

    /// The INNERMOST frame is the last one, and an inlined callee inside it
    /// beats the artifact's own bci — that callee is where the null was
    /// dereferenced.
    #[test]
    fn the_innermost_site_is_the_last_frame_and_its_first_inline_level() {
        let site = innermost_trap_site(&[
            frame("P.outerMost:()V", 3, vec![]),
            frame("P.holder:()I", 7, vec![level("P.leaf:()I", 1)]),
        ])
        .expect("a site");
        assert_eq!(site.class_name, "P");
        assert_eq!(site.method_name, "leaf");
        assert_eq!(site.method_descriptor, "()I");
        assert_eq!(site.bci, 1);
    }

    /// With no inline chain the artifact's own bci is the answer.
    #[test]
    fn an_artifact_with_no_inlined_callee_answers_with_its_own_bci() {
        let site = innermost_trap_site(&[frame("a/b/C.m:(I)J", 12, vec![])]).expect("a site");
        assert_eq!(site.class_name, "a/b/C");
        assert_eq!(site.method_name, "m");
        assert_eq!(site.method_descriptor, "(I)J");
        assert_eq!(site.bci, 12);
    }

    /// `-1` is the "no usable safepoint id" refusal, not a bci. Accepting it
    /// would index `code[usize::MAX]` on one reading and, on the reading that
    /// clamps, produce a message about a completely different opcode.
    #[test]
    fn a_frame_with_no_recovered_bci_is_declined_rather_than_guessed() {
        assert_eq!(
            innermost_trap_site(&[frame("a/b/C.m:()V", -1, vec![])]),
            None
        );
        assert_eq!(innermost_trap_site(&[]), None);
    }

    /// A bci past JVMS 4.9.1's `code_length` bound is refused for the same
    /// reason `stackwalker::inlined_frame_entry` refuses it: the lookups
    /// downstream answer confidently for an out-of-range index.
    #[test]
    fn a_bci_outside_the_jvms_bound_is_refused() {
        assert_eq!(
            innermost_trap_site(&[frame("a/b/C.m:()V", MAX_CODE_LENGTH as i32, vec![])]),
            None
        );
    }

    /// An unparsable label yields no site at all rather than a mangled name.
    #[test]
    fn an_unparsable_label_yields_no_site() {
        assert_eq!(
            innermost_trap_site(&[frame("no-colon-here", 0, vec![])]),
            None
        );
        assert_eq!(innermost_trap_site(&[frame("nodot:()V", 0, vec![])]), None);
        assert_eq!(innermost_trap_site(&[frame(".m:()V", 0, vec![])]), None);
        assert_eq!(innermost_trap_site(&[frame("C.:()V", 0, vec![])]), None);
    }

    /// A chain recovered from the coarse key names the same site an EXACT one
    /// does -- it is what the trace already prints for this frame -- but it is
    /// not a proof, so it does not make the site exact and the caller still has
    /// to corroborate it.
    #[test]
    fn a_coarse_chain_names_the_site_but_does_not_make_it_exact() {
        let coarse = innermost_trap_site(&[frame_coarse_chain(
            "P.caller:()V",
            11,
            vec![level("P.callee:()I", 2)],
        )])
        .expect("a coarse chain still names a site");
        assert_eq!(
            (coarse.method_name.as_str(), coarse.bci, coarse.exact),
            ("callee", 2, false)
        );
        let site =
            innermost_trap_site(&[frame("P.caller:()V", 11, vec![level("P.callee:()I", 2)])])
                .expect("an exact chain names a site");
        assert_eq!(
            (site.method_name.as_str(), site.bci, site.exact),
            ("callee", 2, true)
        );
    }

    /// An artifact with NO splice at the trapping point answers from its own
    /// safepoint-id bci -- the shape the `invoke` warrant is built for -- and
    /// says plainly that the site is not an emitter-described one.
    #[test]
    fn an_unspliced_artifact_answers_from_its_own_bci_and_is_not_exact() {
        let site = innermost_trap_site(&[frame("P.m:()V", 3, vec![])]).expect("a site");
        assert_eq!((site.bci, site.exact), (3, false));
    }

    /// The two warrants are independent: an emitter-described site needs no
    /// action code, and a corroborating action code needs no described site.
    #[test]
    fn a_site_is_trusted_on_either_warrant_and_on_neither_is_refused() {
        use cratonvm_jit_api::npe_action as a;
        let inexact = TrapSite {
            class_name: "C".into(),
            method_name: "m".into(),
            method_descriptor: "()V".into(),
            bci: 4,
            exact: false,
        };
        let exact = TrapSite {
            exact: true,
            ..inexact.clone()
        };
        // getfield, no action recorded: only an emitter-described site will do.
        assert!(!site_is_trustworthy(&inexact, 0xb4, a::NONE));
        assert!(site_is_trustworthy(&exact, 0xb4, a::NONE));
        // arraylength corroborated by the recorded action.
        assert!(site_is_trustworthy(&inexact, 0xbe, a::ARRAY_LENGTH));
        // ... and the SAME action against a different opcode is not
        // corroboration, it is a contradiction.
        assert!(!site_is_trustworthy(&inexact, 0x2e, a::ARRAY_LENGTH));
        // `INVOKE_RECEIVER` corroborates all three invoke opcodes and nothing
        // else -- `invokestatic` (0xb8) included, which has no receiver and
        // can never raise this NPE.
        for op in [0xb6u8, 0xb7, 0xb9] {
            assert!(site_is_trustworthy(&inexact, op, a::INVOKE_RECEIVER));
        }
        assert!(!site_is_trustworthy(&inexact, 0xb8, a::INVOKE_RECEIVER));
        assert!(!site_is_trustworthy(&inexact, 0xb4, a::INVOKE_RECEIVER));
    }

    /// The action-to-opcode table is the corroboration, so a wrong row in it
    /// would silently license exactly the message this guard exists to refuse.
    #[test]
    fn every_array_action_names_its_own_opcode() {
        use cratonvm_jit_api::npe_action as a;
        for (code, opcode) in [
            (a::ARRAY_LENGTH, 0xbeu8),
            (a::ALOAD_INT, 0x2e),
            (a::ALOAD_LONG, 0x2f),
            (a::ALOAD_FLOAT, 0x30),
            (a::ALOAD_DOUBLE, 0x31),
            (a::ALOAD_OBJECT, 0x32),
            (a::ALOAD_BYTE, 0x33),
            (a::ALOAD_CHAR, 0x34),
            (a::ALOAD_SHORT, 0x35),
            (a::ASTORE_INT, 0x4f),
            (a::ASTORE_LONG, 0x50),
            (a::ASTORE_FLOAT, 0x51),
            (a::ASTORE_DOUBLE, 0x52),
            (a::ASTORE_OBJECT, 0x53),
            (a::ASTORE_BYTE, 0x54),
            (a::ASTORE_CHAR, 0x55),
            (a::ASTORE_SHORT, 0x56),
        ] {
            assert!(
                action_admits_opcode(code, opcode),
                "action {code} must admit opcode {opcode:#x}"
            );
            // And nothing else: every action is admitted by ONE opcode.
            assert!(!action_admits_opcode(code, opcode.wrapping_add(1)));
        }
        // `NONE` describes nothing and must corroborate nothing.
        for opcode in [0xb4u8, 0xb5, 0xb6, 0xbe, 0x2e, 0x4f] {
            assert!(!action_admits_opcode(a::NONE, opcode));
        }
        // An array action must not be admitted by an invoke, and vice versa.
        assert!(!action_admits_opcode(a::ARRAY_LENGTH, 0xb6));
        assert!(!action_admits_opcode(a::INVOKE_RECEIVER, 0xbe));
    }

    /// Both array families are laid out `i l f d a b c s`, and this is the
    /// table that depends on it.
    #[test]
    fn the_array_element_table_follows_the_jvms_opcode_order() {
        assert_eq!(array_elem_kind(0), ArrayElemKind::Int);
        assert_eq!(array_elem_kind(1), ArrayElemKind::Long);
        assert_eq!(array_elem_kind(2), ArrayElemKind::Float);
        assert_eq!(array_elem_kind(3), ArrayElemKind::Double);
        assert_eq!(array_elem_kind(4), ArrayElemKind::Object);
        assert_eq!(array_elem_kind(5), ArrayElemKind::Byte);
        assert_eq!(array_elem_kind(6), ArrayElemKind::Char);
        assert_eq!(array_elem_kind(7), ArrayElemKind::Short);
    }
}
