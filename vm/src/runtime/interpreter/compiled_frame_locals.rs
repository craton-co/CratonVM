// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane JVMTIFRAME;
//! `docs/known-issues/jit/j27-jvmti-compiled-frames-answer-opaque-20261028.md`):
//! the locals of a compiled activation, and of an interpreter frame whose body
//! runs compiled (an OSR'd loop), for the C JVMTI table's `GetLocal*` /
//! `GetLocalInstance`, read on the OWNING thread from the activation's home
//! slots -- the reader JDWP's `StackFrame.GetValues` already uses for a parked
//! thread (`read_compiled_homes`, `FrameWordReader::Own`; interpreter round i6
//! wave 120, stage 2 of
//! `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`).
//!
//! HotSpot 25 answers these functions on a compiled frame from the frame's
//! debug info (`compiledVFrame::locals`, `VM_GetOrSetLocal`); this VM's twin
//! of that debug info is the per-safepoint home record a compile made under a
//! debugger keeps (`CompiledMethod::debugger_home_locals`, recorded only under
//! `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`). Fail closed everywhere: no
//! record, an `Unknown` local, a reference word no active oop map names, an
//! inlined callee standing at the point, or two activations the row could
//! name all leave the frame `OPAQUE_FRAME`, as before.
//!
//! Switch: `CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS` (default OFF, read per
//! call; the debugger-only cold path). It serves nothing unless the compile
//! also recorded homes, so the arm to measure sets both switches.

use crate::jit::conservative_roots::{ActiveCompiledFrame, FrameWordReader};
use crate::runtime::frame::Frame;
use crate::types::Value;

/// `CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS`: may the C JVMTI table read a
/// compiled activation's (or an OSR'd frame's) locals from its home slots?
/// Default OFF (opt-in: unmeasured on a binary; see the module note).
pub(crate) fn c_compiled_frame_locals_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS")
}

/// Which activation a listing row with no readable interpreter frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeActivation {
    /// A compiled activation, by its JIT entry-chain index
    /// (`BlockedRow::chain`; several nested activations of one entry share it).
    Compiled { chain: u32 },
    /// Interpreter frame `frame` (from the bottom) whose body runs compiled:
    /// the OSR continuation the listing's dedupe folded into it.
    OsrBody { frame: usize },
}

/// A listing row whose locals a home read may serve: the activation it is,
/// and what the row says of it (class id, JDWP method id, location), which
/// the activation found must match exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HomeRow {
    pub(crate) activation: HomeActivation,
    pub(crate) class_id: u64,
    pub(crate) method_id: u64,
    pub(crate) location: i64,
}

impl HomeRow {
    /// The home row of `raw`, a row of the shared listing
    /// (`listing_rows_of_current_thread`): a compiled activation's row (a
    /// chain index and no frame; an inlined callee's carries no chain) or an
    /// OSR'd interpreter frame's. `None` for every other row.
    pub(crate) fn of_listing_row(raw: &super::BlockedRow) -> Option<Self> {
        let activation = match (raw.frame, raw.chain) {
            (Some(frame), _) if raw.runs_compiled => HomeActivation::OsrBody { frame },
            (None, Some(chain)) => HomeActivation::Compiled { chain },
            _ => return None,
        };
        Some(Self {
            activation,
            class_id: raw.class_id,
            method_id: raw.method_id,
            location: raw.location,
        })
    }

    /// Is `f` the activation this row lists: its class, method and bytecode
    /// index, with nothing inlined at that point (an inlined callee's level
    /// would stand above it, and the home record is the outer frame's only
    /// outside a splice)?
    fn names(&self, f: &ActiveCompiledFrame) -> bool {
        u64::from(f.owner_class_id) == self.class_id
            && i64::from(f.bci) == self.location
            && f.chain().is_empty()
            && label_method_id(&f.label) == Some(self.method_id)
    }
}

/// The JDWP method id of a `"class/Name.method:descriptor"` label, as the
/// listing names a compiled row (`compiled_view_of_with`'s row).
fn label_method_id(label: &str) -> Option<u64> {
    let (owner_and_name, descriptor) = label.rsplit_once(':')?;
    let (_, name) = owner_and_name.rsplit_once('.')?;
    (!name.is_empty() && !descriptor.is_empty())
        .then(|| crate::debug::jdwp_method_id(name, descriptor))
}

/// The one activation among `candidates` that `row` names, or `None` when
/// none or several do (a recursion the walk cannot tell apart: reading
/// either could answer the other frame's values).
fn the_one_named(candidates: Vec<ActiveCompiledFrame>, row: &HomeRow) -> Option<ActiveCompiledFrame> {
    let mut named = candidates.into_iter().filter(|f| row.names(f));
    let first = named.next()?;
    named.next().is_none().then_some(first)
}

/// The activation `row` lists among the CALLING thread's compiled frames
/// over its interpreter frames `frames`, split the way the listing split
/// them (`stackwalker::drop_osr_continuations_keeping`: the kept activations
/// are the compiled rows, the authoritative arm's dropped OSR continuations
/// the OSR'd frames).
fn own_activation(frames: &[Frame], row: &HomeRow) -> Option<ActiveCompiledFrame> {
    let all = crate::jit::conservative_roots::active_compiled_frames();
    let (kept, _, holders) =
        crate::runtime::stackwalker::drop_osr_continuations_keeping(frames, all, true);
    let candidates: Vec<ActiveCompiledFrame> = match row.activation {
        HomeActivation::Compiled { chain } => {
            kept.into_iter().filter(|f| f.chain_index == chain).collect()
        }
        HomeActivation::OsrBody { frame } => holders
            .into_iter()
            .filter(|&(at, _)| at == frame)
            .map(|(_, f)| f)
            .collect(),
    };
    the_one_named(candidates, row)
}

/// The locals of `row`'s activation, one per slot, read from its home slots
/// on the CALLING thread, whose interpreter frames are `frames`; `None` (the
/// frame stays `OPAQUE_FRAME`) when the switch is off, no compiled code is
/// active, or the activation or any of its locals cannot be vouched for
/// ([`super::read_compiled_homes`]). A never-written slot is
/// `Value::Uninitialized`.
///
/// The caller must not reach a safepoint between this read and its last use
/// of a reference answered: such a value is an address the activation's
/// precise roots hold, valid until the next collection (the caller is the
/// owning thread inside its foreign-JNI entry, or a thread parked for the
/// debugger between two polls, as `publish_debugger_frames_under` reads them).
pub(crate) fn own_home_values(frames: &[Frame], row: &HomeRow) -> Option<HomeValues> {
    if !c_compiled_frame_locals_enabled()
        || crate::jit::conservative_roots::current_thread_jit_depth() == 0
    {
        return None;
    }
    let f = own_activation(frames, row)?;
    let homes = super::read_compiled_homes(&f, FrameWordReader::Own)?;
    Some(HomeValues {
        values: homes.iter().map(|&h| super::compiled_home_value(h)).collect(),
        compile_cp_stamp: f.compile_cp_stamp,
    })
}

/// What [`own_home_values`] read: one value per local, and the activation's
/// `compile_cp_stamp` (the redefinition count its compile began at), so the
/// caller can tell a body compiled before its class's last redefinition --
/// an obsolete activation, whose bytecode the class's current
/// `LocalVariableTable` may not describe (j27 wave 8, lane W8REV; item 5 of
/// the JVMTIFRAME page).
pub(crate) struct HomeValues {
    pub(crate) values: Box<[Value]>,
    pub(crate) compile_cp_stamp: Option<u64>,
}

/// j27 wave 8 (lane W8INTR; item 3 of
/// `j27-jvmti-compiled-frames-answer-opaque-20261028.md`): JDWP's
/// `StackFrame.GetValues` of an OSR'd frame. The frame stays listed as it was
/// (opaque, `DebugState::opaque_frames` = `false`: every write and pop still
/// refuses it), but its CURRENT locals, read from its OSR body's home slots
/// by [`own_home_values`], are published, which
/// `commands::frame_has_no_readable_locals` lets a read answer from -- the
/// compiled-home rows' shape (`ListingRow::CompiledHome`). The frame's own
/// `Frame` holds the values of the compiled entry and is never read.
///
/// Must run on the OWNING thread right after its listing was published
/// (`publish_debugger_frames_under`), before it can reach a safepoint: the
/// references read are exported pinned here (`snapshot_local`). The
/// blocked-window publishers are not the owner and do not call it. Same
/// switch as the C table's reads (`CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS`,
/// default OFF); nothing is read without it.
pub(crate) fn publish_osr_home_locals(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    ds: &mut crate::debug::DebugState,
    tid: u64,
) {
    if !c_compiled_frame_locals_enabled() {
        return;
    }
    let rows = crate::jvmti::native_env::published_rows(ds, tid, Some(frames.len()));
    let entries = ds.thread_frames.get(&tid).map(Vec::as_slice).unwrap_or_default();
    for (frame_id, row) in osr_home_rows(entries, rows.unwrap_or_default()) {
        let Some(homes) = own_home_values(frames, &row) else {
            continue;
        };
        let locals = homes
            .values
            .iter()
            .map(|&v| crate::debug::snapshot_local(shared, ds, v))
            .collect();
        ds.update_frame_locals(tid, frame_id, locals);
    }
}

/// The OSR'd frames of a published listing (`entries`, top first, and
/// `rows`, the same listing as `native_env::published_rows` reads it: one
/// row per entry, in order), each with its JDWP frame id and the home row
/// [`own_home_values`] reads it by.
fn osr_home_rows(
    entries: &[crate::debug::FrameEntry],
    rows: Vec<super::BlockedRow>,
) -> Vec<(u64, HomeRow)> {
    entries
        .iter()
        .zip(rows)
        .filter(|(_, raw)| raw.runs_compiled)
        .filter_map(|(e, raw)| Some((e.frame_id, HomeRow::of_listing_row(&raw)?)))
        .collect()
}

#[cfg(test)]
mod j27_jvmtiframe_home_row_tests {
    //! j27 wave 5 (lane JVMTIFRAME): which listing rows a home read may
    //! serve, and which activation it reads.
    use super::*;
    use std::sync::Arc;

    fn raw(frame: Option<usize>, runs_compiled: bool, chain: Option<u32>) -> super::super::BlockedRow {
        super::super::BlockedRow {
            class_id: 7,
            method_id: crate::debug::jdwp_method_id("mid", "(I)I"),
            location: 12,
            frame,
            runs_compiled,
            chain,
        }
    }

    fn activation(label: &str, class_id: u32, bci: i32, chain_index: u32) -> ActiveCompiledFrame {
        ActiveCompiledFrame {
            inline_chain_shared: None,
            interp_depth: 1,
            chain_index,
            label: Arc::from(label),
            owner_class_id: class_id,
            cm_ptr: 0,
            bci,
            inline_chain: Vec::new(),
            chain_exact: false,
            trap_site_exact: false,
            chain_poisoned: false,
            compile_cp_stamp: None,
            fast_throw_c2_tier: false,
            frame_rbp: None,
        }
    }

    #[test]
    fn a_compiled_row_and_an_osr_frame_are_home_rows_and_nothing_else_is() {
        let compiled = HomeRow::of_listing_row(&raw(None, false, Some(3))).expect("compiled row");
        assert_eq!(compiled.activation, HomeActivation::Compiled { chain: 3 });
        assert_eq!((compiled.class_id, compiled.location), (7, 12));
        let osr = HomeRow::of_listing_row(&raw(Some(2), true, None)).expect("OSR'd frame");
        assert_eq!(osr.activation, HomeActivation::OsrBody { frame: 2 });
        // An interpreter frame reads its own `Frame`; a native head, a
        // stand-in level and an inlined callee have no chain and no frame.
        assert_eq!(HomeRow::of_listing_row(&raw(Some(2), false, None)), None);
        assert_eq!(HomeRow::of_listing_row(&raw(None, false, None)), None);
    }

    #[test]
    fn a_label_names_its_jdwp_method_id() {
        assert_eq!(
            label_method_id("p/Q.mid:(I)I"),
            Some(crate::debug::jdwp_method_id("mid", "(I)I"))
        );
        assert_eq!(label_method_id("p/Q.mid"), None);
        assert_eq!(label_method_id("noDot:(I)I"), None);
        assert_eq!(label_method_id("p/Q.:(I)I"), None);
    }

    #[test]
    fn only_a_single_exact_activation_is_read() {
        let row = HomeRow::of_listing_row(&raw(None, false, Some(0))).expect("compiled row");
        let exact = || activation("p/Q.mid:(I)I", 7, 12, 0);
        assert!(the_one_named(vec![exact()], &row).is_some());
        // Another bci, class or method is another frame.
        assert!(the_one_named(vec![activation("p/Q.mid:(I)I", 7, 13, 0)], &row).is_none());
        assert!(the_one_named(vec![activation("p/Q.mid:(I)I", 8, 12, 0)], &row).is_none());
        assert!(the_one_named(vec![activation("p/Q.other:(I)I", 7, 12, 0)], &row).is_none());
        // Two activations the row could name (a recursion): neither.
        assert!(the_one_named(vec![exact(), exact()], &row).is_none());
        // A callee inlined at the point: the homes are not this frame's.
        let mut inlined = exact();
        inlined.inline_chain.push(crate::jit::conservative_roots::InlinedLevel {
            label: Arc::from("p/Q.leaf:()V"),
            class_id: 7,
            bci: 0,
            hidden: None,
        });
        assert!(the_one_named(vec![inlined], &row).is_none());
    }
}

#[cfg(test)]
mod j27_w8intr_jdwp_osr_home_rows_tests {
    //! j27 wave 8 (lane W8INTR): which rows of a published JDWP listing the
    //! OSR'd-frame home read serves, by frame id.
    use super::*;

    fn entry(frame_id: u64) -> crate::debug::FrameEntry {
        crate::debug::FrameEntry {
            frame_id,
            class_id: 7,
            method_id: 9,
            offset: 12,
        }
    }

    fn raw(frame: Option<usize>, runs_compiled: bool, location: i64) -> super::super::BlockedRow {
        super::super::BlockedRow {
            class_id: 7,
            method_id: 9,
            location,
            frame,
            runs_compiled,
            chain: None,
        }
    }

    #[test]
    fn j27_w8intr_only_an_osr_frame_of_the_listing_is_a_jdwp_home_row() {
        // Top first: a native head, a compiled activation (published rows
        // carry no chain), an OSR'd frame (interpreter frame 1) and a plain
        // interpreter frame 0.
        let entries = [entry(40), entry(41), entry(42), entry(43)];
        let rows = vec![
            raw(None, false, -1),
            raw(None, false, 5),
            raw(Some(1), true, 12),
            raw(Some(0), false, 3),
        ];
        let got = osr_home_rows(&entries, rows);
        assert_eq!(got.len(), 1, "{got:?}");
        let (frame_id, row) = got[0];
        assert_eq!(frame_id, 42);
        assert_eq!(row.activation, HomeActivation::OsrBody { frame: 1 });
        assert_eq!((row.class_id, row.method_id, row.location), (7, 9, 12));
    }

    #[test]
    fn j27_w8intr_an_osr_row_without_a_frame_index_is_not_read() {
        // `published_rows` without a depth names no interpreter frame: the
        // read must not guess one.
        let got = osr_home_rows(&[entry(42)], vec![raw(None, true, 12)]);
        assert!(got.is_empty(), "{got:?}");
    }
}
