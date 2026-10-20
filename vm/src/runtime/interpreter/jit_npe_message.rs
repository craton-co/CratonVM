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
    // The suppression gate first: `-XX:-ShowCodeDetailsInExceptionMessages`
    // means "HotSpot's no-message behaviour", which outranks having an answer.
    if crate::runtime::env_cache::helpful_npe_suppressed(&shared.config) {
        return None;
    }
    let site = snapshot.and_then(innermost_trap_site);
    // A trap in a hidden class's code has no message at all, not even the
    // action-only fallback ([`trap_site_is_hidden`]) -- unless
    // `-XX:+ShowHiddenFrames` keeps the frame in the backtrace.
    if !shared.config.show_hidden_frames
        && site
            .as_ref()
            .is_some_and(|site| trap_site_is_hidden(shared, site))
    {
        if dbg() {
            eprintln!("[JITNPE] hidden trap site {site:?}: no message");
        }
        return None;
    }
    let rebuilt = site.and_then(|site| message_at_memoized(shared, &site, action_code));
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
    // function that has always produced it — gate included. The gate check
    // above is then a second ask of the same question, which is worth one
    // more read on a cold path to keep ONE owner of the action-to-message
    // mapping.
    rebuilt.or_else(|| helpful_npe::jit_npe_message_gated(&shared.config, action_code))
}

/// Is `site` in a hidden class's code? HotSpot computes no extended message
/// for such an NPE: `fill_in_stack_trace` leaves the hidden frame out and
/// records that the top frame was hidden, and `JVM_GetExtendedNPEMessage`
/// answers `null` (`get_top_method_and_bci`). The compiled twin of the
/// interpreter's `invoke::npe_frame_is_hidden` (interpreter round i1 wave 52,
/// lane L4; probe `L4W52HiddenReflectiveFailure`, row `hot-hidden-direct`).
/// A hidden class's name ends in a `/0x…` segment, so any other site is
/// answered without the class manager.
#[cold]
fn trap_site_is_hidden(shared: &SharedVm, site: &TrapSite) -> bool {
    if !site
        .class_name
        .rsplit('/')
        .next()
        .is_some_and(|last| last.starts_with("0x"))
    {
        return false;
    }
    let cm = shared.classes.class_manager.read_recursive();
    let recorded = (site.class_id != 0)
        .then(|| ClassId::new(site.class_id))
        .and_then(|id| cm.class_store.get(id))
        .filter(|c| *c.name == *site.class_name);
    match recorded {
        Some(class) => class.hidden,
        None => crate::runtime::stackwalker::find_class_id_by_name_memoized(
            &cm.class_store,
            &site.class_name,
        )
        .and_then(|id| cm.class_store.get(id))
        .is_none_or(|c| c.hidden),
    }
}

/// [`trap_site_is_hidden`] for a trap snapshot's innermost site: a deferred
/// message would be computed from the stored trace's top frame, which the
/// capture's hidden-frame drop has made the CALLER's, so such an NPE is not
/// deferred (`exceptions::npe_message_deferral_admitted`). Never under
/// `-XX:+ShowHiddenFrames`, where the capture keeps the hidden frame on top.
pub(crate) fn snapshot_trap_site_is_hidden(
    shared: &SharedVm,
    snapshot: Option<&[ActiveCompiledFrame]>,
) -> bool {
    if shared.config.show_hidden_frames {
        return false;
    }
    snapshot
        .and_then(innermost_trap_site)
        .is_some_and(|site| trap_site_is_hidden(shared, &site))
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
    /// The `ClassId` the snapshot recorded for the trapping method's class, or
    /// `0` when it recorded none. Preferred over a by-name lookup, which cannot
    /// tell two same-named classes in different loaders apart.
    class_id: u32,
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
    // A safepoint bci the inline-frame map POISONED is the entry of a spliced
    // region: the frame stands inside a callee the map cannot name, and the
    // opcode at that bci is the invoke the splice REPLACED. Read as the trap
    // site, the corroboration below cannot tell it from a real one -- a null
    // receiver inside the splice records `INVOKE_RECEIVER`, and the opcode
    // there IS an invoke -- so the message named the wrong expression: a hot
    // `Field.get(null)` read `Cannot invoke "java.lang.reflect.Field.
    // checkAccess(java.lang.Class, Object)" because "this" is null`, where the
    // null was the `obj.getClass()` inside the spliced `checkAccess` (SBR
    // wave 5, lane reflacc; `tools/probes/SbrW3Reflect.java`). Declined: no
    // message is HotSpot's own hot answer there (C2 drops it), a message
    // about another dereference is nobody's. An exact trap site
    // (`apply_npe_trap_site`) is not a safepoint bci and is unaffected.
    if frame.chain_poisoned && frame.inline_chain.is_empty() && !frame.trap_site_exact {
        return None;
    }
    let spliced = !frame.inline_chain.is_empty();
    let (label, bci, class_id) = match frame.inline_chain.first() {
        Some(level) => (&*level.label, level.bci, level.class_id),
        // `ActiveCompiledFrame::bci` is `-1` for a frame that published no
        // usable safepoint id. That is a refusal, not a bci.
        None => (
            &*frame.label,
            u32::try_from(frame.bci).ok()?,
            frame.owner_class_id,
        ),
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
        class_id,
    })
}

/// Round 15 wave 7 (lane throw2, TH1-5): would [`innermost_trap_site`] answer
/// an EXACT site for `snapshot`? The same tests, without its three `String`s.
///
/// An exact site is the one whose message may be deferred to `getMessage()`
/// (`exceptions::create_compiled_implicit_npe`): the trace the throwable keeps
/// ends with that very program point (`stackwalker::push_compiled_frames`
/// pushes a frame's inline chain innermost LAST, so the innermost stored entry
/// is `inline_chain[0]`, or the frame itself when nothing was inlined), so
/// [`deferred_message_from_top_frame`] reading the trace's top later asks the
/// question [`jit_npe_message`] would ask now, with `exact` set either way --
/// which makes the action code irrelevant to the answer.
pub(crate) fn snapshot_names_an_exact_trap_site(snapshot: Option<&[ActiveCompiledFrame]>) -> bool {
    let Some(frame) = snapshot.and_then(<[ActiveCompiledFrame]>::last) else {
        return false;
    };
    let spliced = !frame.inline_chain.is_empty();
    if !(frame.trap_site_exact || (spliced && frame.chain_exact)) {
        return false;
    }
    let (label, bci): (&str, u32) = match frame.inline_chain.first() {
        Some(level) => (&*level.label, level.bci),
        None => match u32::try_from(frame.bci) {
            Ok(bci) => (&*frame.label, bci),
            Err(_) => return false,
        },
    };
    if bci >= MAX_CODE_LENGTH {
        return false;
    }
    let Some((owner_and_method, descriptor)) = label.rsplit_once(':') else {
        return false;
    };
    let Some((class_name, method_name)) = owner_and_method.rsplit_once('.') else {
        return false;
    };
    !class_name.is_empty() && !method_name.is_empty() && !descriptor.is_empty()
}

/// Round 15 wave 7 (lane throw2, TH1-5): the JEP 358 message of an NPE whose
/// message was deferred, from `top`, the innermost entry of its retained
/// trace -- HotSpot's `JVM_GetExtendedNPEMessage`, which reads the backtrace's
/// top method and bci (`java_lang_Throwable::get_top_method_and_bci`).
///
/// `None` is "no message": the `-XX:-ShowCodeDetailsInExceptionMessages` /
/// opcode gates (asked again here, as HotSpot asks at this point), an entry
/// with no descriptor or no bci, or a bci whose opcode dereferences nothing.
/// The site is EXACT by the deferral's admission
/// ([`snapshot_names_an_exact_trap_site`]), so the action code is not
/// consulted and none is passed.
pub(crate) fn deferred_message_from_top_frame(
    shared: &SharedVm,
    top: &crate::native::registry::StackTraceEntry,
) -> Option<String> {
    if crate::runtime::env_cache::helpful_npe_suppressed(&shared.config) {
        return None;
    }
    let bci = u32::try_from(top.byte_code_index)
        .ok()
        .filter(|bci| *bci < MAX_CODE_LENGTH)?;
    let descriptor = top.method_descriptor.as_deref()?;
    let site = TrapSite {
        // A stored entry carries the internal name (`parse_frame_label`, the
        // cached method's own); the replace only matters for a dotted one.
        class_name: top.class_name.replace('.', "/"),
        method_name: top.method_name.to_string(),
        method_descriptor: descriptor.to_owned(),
        // Widening: bounded above by `MAX_CODE_LENGTH`.
        bci: bci as usize,
        exact: true,
        class_id: top.class_id.map_or(0, |id| id.as_u32()),
    };
    if site.class_name.is_empty()
        || site.method_name.is_empty()
        || site.method_descriptor.is_empty()
    {
        return None;
    }
    message_at_memoized(shared, &site, cratonvm_jit_api::npe_action::NONE)
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
    // The recorded id first: a by-name lookup returns whichever same-named
    // class the store finds first, which for a class defined by two loaders
    // may be the other one -- and then the message describes the other
    // class's bytecode at this bci. The name re-check guards a recycled id.
    let recorded = (site.class_id != 0)
        .then(|| ClassId::new(site.class_id))
        .filter(|id| {
            cm.class_store
                .get(*id)
                .is_some_and(|c| *c.name == *site.class_name)
        });
    let class_id = match recorded {
        Some(id) => id,
        None => crate::runtime::stackwalker::find_class_id_by_name_memoized(
            &cm.class_store,
            &site.class_name,
        )?,
    };
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
        // monitorenter / monitorexit — HotSpot words the two differently.
        0xc2 => (helpful_npe::action_monitor(), 0),
        0xc3 => (super::opcodes::monitorexit_npe_action().to_string(), 0),
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

/// One remembered [`message_at`] answer. Validated against every input the
/// analysis reads: the site's names, bci, exactness and recorded class id, the
/// action code, and a stamp of what can change a class's bytes or a name's
/// meaning (the VM's class definition epoch and the redefinition count).
struct NpeMessageMemo {
    stamp: (u64, u64),
    class_name: String,
    method_name: String,
    method_descriptor: String,
    message: Option<String>,
}

/// `(vm_identity, recorded class id, bci, action code, exact)`.
type NpeMessageKey = (usize, u32, usize, u8, bool);

/// Clear-on-full bound for [`NPE_MESSAGE_MEMO`].
const NPE_MESSAGE_MEMO_CAP: usize = 1024;

thread_local! {
    static NPE_MESSAGE_MEMO: std::cell::RefCell<
        rustc_hash::FxHashMap<NpeMessageKey, NpeMessageMemo>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `CRATONVM_JIT_NPE_MESSAGE_MEMO` -- default ON; `0` rebuilds every message.
/// Read once per VM into `VmConfig::npe_message_memo` (interpreter round i2
/// wave 58, lane L7; a process `OnceLock` before).
#[inline(always)]
fn npe_message_memo_enabled(shared: &SharedVm) -> bool {
    shared.config.npe_message_memo
}

/// [`message_at`], remembered per trapping site on this thread. A compiled
/// loop that NPEs at one site paid the bytecode copy and the backward
/// analysis on every throw (round 12 wave 3, lane exc).
fn message_at_memoized(shared: &SharedVm, site: &TrapSite, action_code: u8) -> Option<String> {
    if !npe_message_memo_enabled(shared) {
        return message_at(shared, site, action_code);
    }
    // Read BEFORE the analysis: a definition landing during it leaves an entry
    // that is already stale.
    let stamp = (
        cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
            .class_definition_epoch(),
        cratonvm_classloading::class_redefinition_count(),
    );
    let key: NpeMessageKey = (
        shared.vm_identity,
        site.class_id,
        site.bci,
        action_code,
        site.exact,
    );
    let hit = NPE_MESSAGE_MEMO.with(|memo| {
        let memo = memo.try_borrow().ok()?;
        let entry = memo.get(&key)?;
        (entry.stamp == stamp
            && entry.class_name == site.class_name
            && entry.method_name == site.method_name
            && entry.method_descriptor == site.method_descriptor)
            .then(|| entry.message.clone())
    });
    if let Some(message) = hit {
        return message;
    }
    let message = message_at(shared, site, action_code);
    NPE_MESSAGE_MEMO.with(|memo| {
        let Ok(mut memo) = memo.try_borrow_mut() else {
            return;
        };
        if memo.len() >= NPE_MESSAGE_MEMO_CAP && !memo.contains_key(&key) {
            memo.clear();
        }
        memo.insert(
            key,
            NpeMessageMemo {
                stamp,
                class_name: site.class_name.clone(),
                method_name: site.method_name.clone(),
                method_descriptor: site.method_descriptor.clone(),
                message: message.clone(),
            },
        );
    });
    message
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
    // JIT round 20 (lane throw): the monitor helpers' null arms.
    if code == a::MONITOR_ENTER {
        return opcode == 0xc2;
    }
    if code == a::MONITOR_EXIT {
        return opcode == 0xc3;
    }
    // JIT round 20 wave 7 (lane fuzzC): `jit_throw_exception`'s null arm.
    if code == a::ATHROW {
        return opcode == 0xbf;
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
/// `baload`/`bastore` cover `byte[]` AND `boolean[]`; the array is null, so
/// nothing can separate them. `ArrayElemKind::Byte` renders as HotSpot's
/// combined spelling `byte/boolean` (see `ArrayElemKind`'s own match), so this
/// cell agrees with the interpreter and with HotSpot.
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
            inline_chain_shared: None,
            interp_depth: 0,
            chain_index: 0,
            label: label.into(),
            owner_class_id: 0,
            cm_ptr: 0,
            bci,
            inline_chain: chain,
            chain_exact: true,
            trap_site_exact: false,
            chain_poisoned: false,
            compile_cp_stamp: None,
            fast_throw_c2_tier: false,
            frame_rbp: None,
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
            label: label.into(),
            bci,
            class_id: 0,
            hidden: None,
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

    /// Round 15 wave 7 (lane throw2, TH1-5): the allocation-free deferral
    /// admission answers exactly `innermost_trap_site`'s `exact`, shape by
    /// shape -- a deferred message is asked of the same site the eager one is.
    #[test]
    fn the_deferral_admission_is_innermost_trap_site_exactness() {
        let trap = |label: &str, bci: i32| ActiveCompiledFrame {
            trap_site_exact: true,
            ..frame(label, bci, vec![])
        };
        let shapes: Vec<Vec<ActiveCompiledFrame>> = vec![
            vec![],
            vec![frame("P.m:()V", 4, vec![])],
            vec![trap("P.m:()V", 4)],
            vec![frame("P.outer:()V", 2, vec![]), trap("P.m:()V", 4)],
            vec![frame("P.holder:()I", 7, vec![level("P.leaf:()I", 1)])],
            vec![frame_coarse_chain(
                "P.holder:()I",
                7,
                vec![level("P.leaf:()I", 1)],
            )],
            vec![frame("P.holder:()I", 7, vec![level("P.leaf:()I", 70_000)])],
            vec![trap("P.m:()V", -1)],
            vec![trap("garbage", 3)],
            vec![trap(".m:()V", 3)],
        ];
        for (i, shape) in shapes.iter().enumerate() {
            let want = innermost_trap_site(shape).is_some_and(|site| site.exact);
            assert_eq!(
                snapshot_names_an_exact_trap_site(Some(shape)),
                want,
                "shape {i}"
            );
        }
        assert!(!snapshot_names_an_exact_trap_site(None));
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

    /// A POISONED safepoint bci (the entry of a spliced region with several
    /// calls) is declined: the opcode there is the invoke the splice replaced,
    /// not the dereference that trapped. An exact trap site at the same bci,
    /// or a frame that did recover a chain, still answers.
    #[test]
    fn a_poisoned_splice_entry_bci_is_declined() {
        let poisoned = ActiveCompiledFrame {
            chain_poisoned: true,
            ..frame(
                "java/lang/reflect/Field.get:(Ljava/lang/Object;)Ljava/lang/Object;",
                14,
                vec![],
            )
        };
        assert_eq!(innermost_trap_site(std::slice::from_ref(&poisoned)), None);
        let exact = ActiveCompiledFrame {
            trap_site_exact: true,
            ..poisoned.clone()
        };
        assert_eq!(innermost_trap_site(&[exact]).map(|s| s.bci), Some(14));
        let chained = ActiveCompiledFrame {
            inline_chain: vec![level("P.leaf:()I", 2)],
            ..poisoned
        };
        assert_eq!(innermost_trap_site(&[chained]).map(|s| s.bci), Some(2));
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

    /// The site carries the class id the snapshot recorded for the trapping
    /// method -- the artifact owner's when unspliced, the inlined level's when
    /// spliced -- so the bytecode lookup need not guess by name.
    #[test]
    fn the_site_carries_the_recorded_class_id_of_the_trapping_method() {
        let mut owner = frame("P.m:()V", 3, vec![]);
        owner.owner_class_id = 41;
        assert_eq!(innermost_trap_site(&[owner.clone()]).unwrap().class_id, 41);
        let mut inlined = level("Q.leaf:()I", 1);
        inlined.class_id = 57;
        owner.inline_chain = vec![inlined];
        assert_eq!(innermost_trap_site(&[owner.clone()]).unwrap().class_id, 57);
        // A level that recorded no id stays "unknown"; it does NOT inherit the
        // artifact owner's, which names a different class.
        owner.inline_chain = vec![level("Q.leaf:()I", 1)];
        assert_eq!(innermost_trap_site(&[owner]).unwrap().class_id, 0);
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
            class_id: 0,
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

#[cfg(test)]
mod jr20_monitor_action_tests {
    use super::action_admits_opcode;
    use cratonvm_jit_api::npe_action as a;

    /// JIT round 20 (lane throw): the monitor helpers' action codes
    /// corroborate exactly their own opcode, so a stale bci at another opcode
    /// still declines.
    #[test]
    fn monitor_actions_admit_only_their_opcode() {
        assert!(action_admits_opcode(a::MONITOR_ENTER, 0xc2));
        assert!(!action_admits_opcode(a::MONITOR_ENTER, 0xc3));
        assert!(!action_admits_opcode(a::MONITOR_ENTER, 0xb4));
        assert!(action_admits_opcode(a::MONITOR_EXIT, 0xc3));
        assert!(!action_admits_opcode(a::MONITOR_EXIT, 0xc2));
        assert!(!action_admits_opcode(a::NONE, 0xc2));
    }

    /// JIT round 20 wave 7 (lane fuzzC; `R20FuzzCAthrowNull1`): the compiled
    /// `athrow`-on-null action corroborates exactly `athrow`, so a stale bci at
    /// an invoke (the frame's last call) still declines.
    #[test]
    fn the_athrow_action_admits_only_athrow() {
        assert!(action_admits_opcode(a::ATHROW, 0xbf));
        assert!(!action_admits_opcode(a::ATHROW, 0xb6));
        assert!(!action_admits_opcode(a::ATHROW, 0xc2));
        assert!(!action_admits_opcode(a::NONE, 0xbf));
    }
}
