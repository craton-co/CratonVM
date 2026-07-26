// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! stw-residual-close (CRATONVM_DBG_ZERO_RANGES): forensic ring of bulk
//! memory-zeroing events — (site, tag, start, len) — so a stale-ref capture
//! can name WHICH subsystem zeroed an address and roughly when. Sites:
//! 1 = non-moving sweep dead-span zero+freelist (tag = sweep cycle),
//! 2 = `Arena::reset` full wipe (moving-GC from-space reset; tag = 0).
//! Debug-only; every entry point is gated and free when the env is unset.

use crate::gc_flags;
use std::sync::{Mutex, OnceLock};

pub fn enabled() -> bool {
    gc_flags().dbg_zero_ranges
}

#[allow(clippy::type_complexity)]
fn ring() -> &'static Mutex<Vec<(u8, u32, usize, usize)>> {
    static R: OnceLock<Mutex<Vec<(u8, u32, usize, usize)>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

const CAP: usize = 150_000;

pub fn record(site: u8, tag: u32, start: usize, len: usize) {
    if !enabled() || len == 0 {
        return;
    }
    if let Ok(mut r) = ring().lock() {
        if r.len() >= CAP {
            let drop_n = CAP / 4;
            r.drain(..drop_n);
        }
        r.push((site, tag, start, len));
    }
}

/// Ranges containing `addr`, oldest first: (age-index-from-end, site, tag,
/// start, len).
#[allow(clippy::type_complexity)]
pub fn probe(addr: usize) -> Vec<(usize, u8, u32, usize, usize)> {
    if !enabled() {
        return Vec::new();
    }
    match ring().lock() {
        Ok(r) => {
            let n = r.len();
            r.iter()
                .enumerate()
                .filter(|(_, (_, _, s, l))| addr >= *s && addr < *s + *l)
                .map(|(i, (site, tag, s, l))| (n - i, *site, *tag, *s, *l))
                .collect()
        }
        Err(_) => Vec::new(),
    }
}
