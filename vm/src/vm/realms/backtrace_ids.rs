// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A throwable's captured backtrace held BY THE THROWABLE: the per-VM frame
//! ids its holder array names.
//!
//! Interpreter round i3 wave 82, lane L2
//! (`CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1`, default OFF;
//! `docs/internal/fixed-bugs/interpreter-i73-L2-proposal-backtrace-owned-by-the-throwable-KEPT-OFF-20261115.md`).
//!
//! HotSpot's `java_lang_Throwable::fill_in_stack_trace` stores the backtrace
//! IN the throwable (`Throwable.backtrace`, an `Object[]` chunk of method ids
//! and bcis) and builds no `StackTraceElement` until Java asks
//! (`StackTraceElement.of(backtrace, depth)`). The collector treats it as any
//! other field: no lock, no table, no sweep.
//!
//! With the switch off this VM keeps the frames in the address-keyed
//! registry (`SharedVm::store_throwable_backtrace`, `ThrowableStackShard`)
//! and parks a self-reference in `backtrace` as the non-null marker; every
//! collection must remap and sweep that registry, and a dead throwable's row
//! lives until then. With the switch on, a capture whose frames all have a
//! frame id is written into a Java `long[]` (the *holder*) stored in
//! `Throwable.backtrace`:
//!
//! ```text
//! [0] HOLDER_MAGIC
//! [1] captured_at   (class_redefinition_count() at capture)
//! [2..] (frame id << 32) | (bci as u32), OUTERMOST-first as the registry
//! ```
//!
//! A frame id names one [`BacktraceFrame`] *template* (the frame with its bci
//! ignored) in this per-VM table, keyed by the identity of the shared record
//! the frame already holds (the `CachedBytecodeMethod` `Arc`, a compiled
//! artifact's label `Arc`), which the table keeps alive -- so an address in a
//! key is never reused while the key exists. The table only grows: one row per
//! distinct (method, class) / (label, class, stamp) that ever appeared in a
//! trace, capped at [`FRAME_ID_CAP`]. An already-built
//! [`BacktraceFrame::Entry`] has no shared identity, and a trace holding one
//! (a reflective / uncached push, a splice, a stand-in) keeps the registry.
//!
//! What the holder buys: a capture takes no registry write lock, a reader no
//! shard lock or address probe, and a dead throwable's frames die with it in
//! the collection that frees it, at no cost to the pause (the old registry's
//! sweep is proportional to the throwables allocated since the last one).

use crate::runtime::stackwalker::BacktraceFrame;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// Word 0 of every holder: "CRBTRACE". A `long[]` the VM did not write cannot
/// reach `Throwable.backtrace` (a private field of `java.base`), but the
/// readers also accept the holder itself (`StackTraceElement.of(backtrace,
/// depth)` hands it to `initStackTraceElements`), and the word keeps them
/// from decoding any other `long[]`.
pub(crate) const HOLDER_MAGIC: u64 = 0x4352_4254_5241_4345;

/// The header words before the first frame word.
pub(crate) const HOLDER_HEADER_WORDS: usize = 2;

/// The most frame ids one VM hands out. A miss past it keeps the registry for
/// that capture.
pub(crate) const FRAME_ID_CAP: usize = 1 << 20;

/// The identity of a [`BacktraceFrame`] apart from its bci.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum FrameIdKey {
    Method {
        method: usize,
        class_id: u32,
    },
    Compiled {
        label: usize,
        owner_class_id: u32,
        cp_stamp: Option<u64>,
    },
    Inlined {
        label: usize,
        class_id: u32,
        cp_stamp: Option<u64>,
        /// `BacktraceFrame::Inlined::hidden` (wave 122, L2): a template
        /// must give back the stamp it was registered with.
        hidden: Option<bool>,
    },
}

/// `frame`'s key and its bci's 32 bits, or `None` for an
/// [`BacktraceFrame::Entry`].
fn key_of(frame: &BacktraceFrame) -> Option<(FrameIdKey, u32)> {
    match frame {
        BacktraceFrame::Method {
            method,
            bci,
            class_id,
        } => Some((
            FrameIdKey::Method {
                method: Arc::as_ptr(method) as usize,
                class_id: class_id.as_u32(),
            },
            // Cast: the bit pattern; `with_bci` casts it back.
            *bci as u32,
        )),
        BacktraceFrame::Compiled {
            label,
            owner_class_id,
            bci,
            cp_stamp,
        } => Some((
            FrameIdKey::Compiled {
                label: Arc::as_ptr(label).cast::<u8>() as usize,
                owner_class_id: *owner_class_id,
                cp_stamp: *cp_stamp,
            },
            // Cast: the bit pattern (`-1` = none survives the round trip).
            *bci as u32,
        )),
        BacktraceFrame::Inlined {
            label,
            class_id,
            bci,
            cp_stamp,
            hidden,
        } => Some((
            FrameIdKey::Inlined {
                label: Arc::as_ptr(label).cast::<u8>() as usize,
                class_id: *class_id,
                cp_stamp: *cp_stamp,
                hidden: *hidden,
            },
            *bci,
        )),
        BacktraceFrame::Entry(_) => None,
    }
}

/// `template` with the bci `bits` names ([`key_of`]'s inverse).
fn with_bci(template: &BacktraceFrame, bits: u32) -> BacktraceFrame {
    match template {
        BacktraceFrame::Method {
            method, class_id, ..
        } => BacktraceFrame::Method {
            method: Arc::clone(method),
            // Cast: back from `key_of`'s bit pattern.
            bci: bits as i32,
            class_id: *class_id,
        },
        BacktraceFrame::Compiled {
            label,
            owner_class_id,
            cp_stamp,
            ..
        } => BacktraceFrame::Compiled {
            label: Arc::clone(label),
            owner_class_id: *owner_class_id,
            // Cast: back from `key_of`'s bit pattern.
            bci: bits as i32,
            cp_stamp: *cp_stamp,
        },
        BacktraceFrame::Inlined {
            label,
            class_id,
            cp_stamp,
            hidden,
            ..
        } => BacktraceFrame::Inlined {
            label: Arc::clone(label),
            class_id: *class_id,
            bci: bits,
            cp_stamp: *cp_stamp,
            hidden: *hidden,
        },
        // Never a template (`key_of` refuses it); kept total.
        BacktraceFrame::Entry(entry) => BacktraceFrame::Entry(entry.clone()),
    }
}

#[inline]
fn pack(id: u32, bits: u32) -> u64 {
    (u64::from(id) << 32) | u64::from(bits)
}

#[derive(Default)]
struct FrameIdTable {
    by_key: FxHashMap<FrameIdKey, u32>,
    /// Indexed by frame id. Holds the `Arc`s the keys name.
    templates: Vec<BacktraceFrame>,
}

/// The per-VM frame-id table (see the module doc). A leaf lock: nothing is
/// taken under it, and no Java code runs while it is held.
pub(crate) struct BacktraceFrameIds {
    table: RwLock<FrameIdTable>,
}

impl Default for BacktraceFrameIds {
    fn default() -> Self {
        Self::new()
    }
}

impl BacktraceFrameIds {
    pub(crate) fn new() -> Self {
        Self {
            table: RwLock::new(FrameIdTable::default()),
        }
    }

    /// The holder words for `frames` (header included), or `None` when a
    /// frame has no id and the table cannot give it one (an
    /// [`BacktraceFrame::Entry`], or the table at [`FRAME_ID_CAP`]): the
    /// caller keeps the registry for this capture.
    ///
    /// One shared read lock for a trace whose frames all have ids (every
    /// repeat throw); the first capture through a new frame takes the write
    /// lock once and encodes again.
    pub(crate) fn encode(&self, frames: &[BacktraceFrame], captured_at: u64) -> Option<Vec<u64>> {
        let mut words = Vec::with_capacity(HOLDER_HEADER_WORDS + frames.len());
        words.push(HOLDER_MAGIC);
        words.push(captured_at);
        {
            let table = self.table.read();
            let mut complete = true;
            for frame in frames {
                let (key, bits) = key_of(frame)?;
                match table.by_key.get(&key).copied() {
                    Some(id) => words.push(pack(id, bits)),
                    None => {
                        complete = false;
                        break;
                    }
                }
            }
            if complete {
                return Some(words);
            }
        }
        words.truncate(HOLDER_HEADER_WORDS);
        let mut guard = self.table.write();
        let table = &mut *guard;
        for frame in frames {
            let (key, bits) = key_of(frame)?;
            let found = table.by_key.get(&key).copied();
            let id = match found {
                Some(id) => id,
                None => {
                    if table.templates.len() >= FRAME_ID_CAP {
                        return None;
                    }
                    let id = u32::try_from(table.templates.len()).ok()?;
                    table.templates.push(frame.clone());
                    table.by_key.insert(key, id);
                    id
                }
            };
            words.push(pack(id, bits));
        }
        Some(words)
    }

    /// The frames `frame_words` (the words after the header) name, or `None`
    /// when a word names no id this table handed out.
    pub(crate) fn decode(&self, frame_words: &[u64]) -> Option<Box<[BacktraceFrame]>> {
        let table = self.table.read();
        frame_words
            .iter()
            .map(|&word| {
                // Cast: the two halves `pack` wrote.
                let id = (word >> 32) as usize;
                let bits = word as u32;
                table.templates.get(id).map(|t| with_bci(t, bits))
            })
            .collect()
    }

    /// Frame ids handed out so far (tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.table.read().templates.len()
    }
}

/// Is `words` a holder's contents (the magic word, a stamp, any frames)?
#[inline]
pub(crate) fn is_holder_words(first_word: u64, len: usize) -> bool {
    first_word == HOLDER_MAGIC && len >= HOLDER_HEADER_WORDS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::registry::StackTraceEntry;

    fn compiled(label: &Arc<str>, owner: u32, bci: i32) -> BacktraceFrame {
        BacktraceFrame::Compiled {
            label: Arc::clone(label),
            owner_class_id: owner,
            bci,
            cp_stamp: None,
        }
    }

    fn inlined(label: &Arc<str>, class_id: u32, bci: u32, stamp: Option<u64>) -> BacktraceFrame {
        BacktraceFrame::Inlined {
            label: Arc::clone(label),
            class_id,
            bci,
            cp_stamp: stamp,
            hidden: None,
        }
    }

    fn shape(frames: &[BacktraceFrame]) -> Vec<String> {
        frames.iter().map(|f| format!("{f:?}")).collect()
    }

    #[test]
    fn a_holder_round_trips_every_frame_and_its_bci() {
        let ids = BacktraceFrameIds::new();
        let a: Arc<str> = Arc::from("p/A.run()V");
        let b: Arc<str> = Arc::from("p/B.call(I)I");
        let frames = vec![
            compiled(&a, 7, -1),
            compiled(&a, 7, 12),
            inlined(&b, 9, 3, Some(4)),
            compiled(&b, 9, 0),
        ];
        let words = ids.encode(&frames, 5).expect("every frame has an id");
        assert_eq!(words.len(), HOLDER_HEADER_WORDS + frames.len());
        assert_eq!(words[0], HOLDER_MAGIC);
        assert_eq!(words[1], 5);
        assert!(is_holder_words(words[0], words.len()));
        // `a` twice at two bcis is one id; three templates in all.
        assert_eq!(ids.len(), 3);
        let back = ids
            .decode(&words[HOLDER_HEADER_WORDS..])
            .expect("known ids");
        assert_eq!(shape(&back), shape(&frames));
        // A second capture through the same frames hands out nothing new.
        let again = ids.encode(&frames[1..], 6).expect("repeat");
        assert_eq!(ids.len(), 3);
        assert_eq!(
            &again[HOLDER_HEADER_WORDS..],
            &words[HOLDER_HEADER_WORDS + 1..]
        );
    }

    #[test]
    fn a_different_stamp_or_class_is_a_different_frame() {
        let ids = BacktraceFrameIds::new();
        let a: Arc<str> = Arc::from("p/A.run()V");
        let frames = vec![
            inlined(&a, 1, 2, None),
            inlined(&a, 1, 2, Some(1)),
            inlined(&a, 2, 2, None),
        ];
        let words = ids.encode(&frames, 0).expect("ids");
        assert_eq!(ids.len(), 3);
        let back = ids
            .decode(&words[HOLDER_HEADER_WORDS..])
            .expect("known ids");
        assert_eq!(shape(&back), shape(&frames));
    }

    #[test]
    fn an_entry_frame_keeps_the_registry() {
        let ids = BacktraceFrameIds::new();
        let a: Arc<str> = Arc::from("p/A.run()V");
        let entry = BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from("p/A"),
            method_name: Arc::from("run"),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: -1,
            class_id: None,
            method_index: None,
        });
        assert!(ids.encode(&[compiled(&a, 1, 0), entry], 0).is_none());
        assert!(ids.encode(&[], 0).is_some(), "an empty trace is a holder");
    }

    #[test]
    fn an_unknown_id_does_not_decode() {
        let ids = BacktraceFrameIds::new();
        assert!(ids.decode(&[pack(0, 0)]).is_none());
        assert_eq!(ids.decode(&[]).map(|f| f.len()), Some(0));
        assert!(!is_holder_words(0, 4));
        assert!(!is_holder_words(HOLDER_MAGIC, 1));
    }
}
