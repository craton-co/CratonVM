// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Diagnostic-only heap field write-watchpoint, gated behind
//! `CRATONVM_DBG_FIELD_WATCH`.
//!
//! Software equivalent of a debugger hardware watchpoint, added to chase
//! the `TestUpgrade` `RootReference`/`MVMap` residual documented in
//! `bug-h2-suite-residual-fail-triage-FIXED.md`.
//! Every construction site of every object in the suspect chain
//! (`Page`/`RootReference`) was already exhaustively traced and found
//! clean, yet the end-to-end field value was still observed wrong once.
//! This module lets any code register an object as "watched" at
//! construction time; [`crate::Heap::set_field`]-equivalents can then
//! report EVERY write to that object's fields, with a running per-slot
//! count. A `final` field's bytecode-level contract is "written exactly
//! once, from within its declaring constructor" — a write count reaching
//! 2 for a watched slot is direct proof of either an aliasing/offset-
//! collision bug or a legitimate-looking GC-relocation copy corrupting
//! the slot, either of which this module surfaces immediately (not
//! suppressed as a duplicate — every write is reported).
//!
//! Pure additive diagnostic: zero cost when the env var is unset (checked
//! once via `OnceLock`), and even when set, only affects objects explicitly
//! registered via [`watch`].

use crate::value::ObjectRef;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static ENABLED: OnceLock<bool> = OnceLock::new();
static WATCHED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// `(obj_ptr, index) -> write count so far`, incremented and returned on
/// every reported write so callers can flag `count >= 2` prominently.
static WRITE_COUNTS: Mutex<Option<HashMap<(usize, usize), u32>>> = Mutex::new(None);

#[inline]
fn enabled() -> bool {
    *ENABLED.get_or_init(|| crate::flags::runtime_var_os("CRATONVM_DBG_FIELD_WATCH").is_some())
}

/// Register `obj` for write tracking. Idempotent. No-op unless
/// `CRATONVM_DBG_FIELD_WATCH` is set.
pub fn watch(obj: ObjectRef) {
    if !enabled() {
        return;
    }
    let mut g = WATCHED.lock().unwrap();
    let p = obj.as_ptr() as usize;
    if !g.contains(&p) {
        g.push(p);
    }
}

/// True if `obj` was previously registered via [`watch`]. Always false
/// unless `CRATONVM_DBG_FIELD_WATCH` is set.
#[inline]
pub fn is_watched(obj: ObjectRef) -> bool {
    if !enabled() {
        return false;
    }
    let g = WATCHED.lock().unwrap();
    g.contains(&(obj.as_ptr() as usize))
}

/// Records a write to `(obj, index)` and returns the write count for that
/// slot INCLUDING this write (so the first call returns 1, the second
/// returns 2, etc.). Callers should log every call; a returned count >= 2
/// is the direct signal a `final` field has been written more than once.
pub fn record_write(obj: ObjectRef, index: usize) -> u32 {
    let mut g = WRITE_COUNTS.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    let entry = map.entry((obj.as_ptr() as usize, index)).or_insert(0);
    *entry += 1;
    *entry
}
