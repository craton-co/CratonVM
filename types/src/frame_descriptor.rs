// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Frame descriptor records: the header layout shared by the optimizing tier
//! (which publishes them) and the collector's shadow-stack walker (which
//! expands them), and the table of interned records. See
//! `cratonvm_gc::shadow_stack`, "Frame descriptor records".

use std::sync::atomic::{AtomicU64, Ordering};

/// High half of a descriptor record's header word.
pub const DESCRIPTOR_MAGIC: u64 = 0xF0B1_0C4D;
/// Most homes one descriptor record may name (the frame block's own ceiling).
pub const DESCRIPTOR_MAX_HOMES: usize = 1024;

/// Words of a descriptor record (`w0`, the header and the packed words) whose
/// header word is `w1`, or `None` when `w1` is not a descriptor header. Pure.
pub fn descriptor_record_words(w1: usize) -> Option<usize> {
    let w1 = w1 as u64;
    if w1 >> 32 != DESCRIPTOR_MAGIC {
        return None;
    }
    let n = (w1 & 0xFFFF_FFFF) as usize;
    if n > DESCRIPTOR_MAX_HOMES {
        return None;
    }
    Some(2 + n.div_ceil(4))
}

/// The header word of a descriptor record of `n` homes.
pub fn descriptor_header(n: usize) -> u64 {
    (DESCRIPTOR_MAGIC << 32) | (n as u64 & 0xFFFF_FFFF)
}

/// Words of the interned-descriptor table (512 KiB).
const INTERNED_DESCRIPTOR_WORDS: usize = 1 << 16;

/// Immutable descriptor records (header and packed words), interned once per
/// distinct record so a frame can publish `[rbp | DESCRIPTOR_TAG, address]`
/// instead of copying the record on every activation. A word is written once,
/// before its address is handed out, and never again.
static INTERNED_DESCRIPTORS: [AtomicU64; INTERNED_DESCRIPTOR_WORDS] =
    [const { AtomicU64::new(0) }; INTERNED_DESCRIPTOR_WORDS];

/// Next free word of [`INTERNED_DESCRIPTORS`] and the records already in it.
static DESCRIPTOR_INTERN_INDEX: std::sync::Mutex<
    Option<(usize, std::collections::HashMap<Vec<u64>, usize>)>,
> = std::sync::Mutex::new(None);

/// The stable address of `record` (a descriptor header followed by its packed
/// home words, the words a record carries after `w0`) in the interned table,
/// or `None` when it is not a whole record or the table is full -- the caller
/// then publishes the record inline.
pub fn intern_descriptor(record: &[u64]) -> Option<usize> {
    let words = descriptor_record_words(*record.first()? as usize)?;
    if words - 1 != record.len() {
        return None;
    }
    let mut guard = DESCRIPTOR_INTERN_INDEX.lock().unwrap_or_else(|e| e.into_inner());
    let (next, index) = guard.get_or_insert_with(Default::default);
    if let Some(&addr) = index.get(record) {
        return Some(addr);
    }
    if *next + record.len() > INTERNED_DESCRIPTOR_WORDS {
        return None;
    }
    for (k, &w) in record.iter().enumerate() {
        INTERNED_DESCRIPTORS[*next + k].store(w, Ordering::Release);
    }
    let addr = INTERNED_DESCRIPTORS[*next].as_ptr() as usize;
    *next += record.len();
    index.insert(record.to_vec(), addr);
    Some(addr)
}

/// `(homes, address of the packed words)` of the interned record at `addr`,
/// or `None` when `addr` is not a record start inside the table. Never reads
/// outside the table, so a mistyped word after a tagged `w0` is harmless.
pub fn interned_descriptor(addr: usize) -> Option<(usize, usize)> {
    let lo = INTERNED_DESCRIPTORS.as_ptr() as usize;
    let off = addr.checked_sub(lo)?;
    if off % 8 != 0 || off / 8 >= INTERNED_DESCRIPTOR_WORDS {
        return None;
    }
    let i = off / 8;
    let header = INTERNED_DESCRIPTORS[i].load(Ordering::Acquire) as usize;
    let words = descriptor_record_words(header)?;
    if i + words - 1 > INTERNED_DESCRIPTOR_WORDS {
        return None;
    }
    Some((header & 0xFFFF_FFFF, addr + 8))
}
