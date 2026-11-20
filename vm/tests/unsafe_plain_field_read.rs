// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A compiled `Unsafe.getInt` / `getLong` of an INSTANCE field is answered by
//! the thin helper, not dispatched to the native.
//!
//! The helper served only primitive arrays and off-heap addresses, so every
//! read of a field -- Netty's `refCnt` check on every `ByteBuf` access -- paid
//! the registered native's whole funnel, about 600 ns, and
//! `AdvancedLeakAwareCompositeByteBufTest`'s 10-thread tests ran 100x slower
//! than HotSpot and timed out at 30 s under load. See
//! `docs/feature-designs/collections-interception.md`.
//!
//! The probe prints HotSpot's output in both modes, and the JIT method-stats census must
//! report the reads as served: `declined` stays a small fraction of `served`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

// each holder: 1000 * 2000 * (6 + 0x08) = 28_000_000
const EXPECTED: &str =
    "cold=28000000 hot=28000000\nafter=9 1122334455667788\nfloat=true\nPLAIN-FIELD-READ-OK\n";

/// `served=<n> declined=<m>` from the method-stats census Unsafe accessor line.
fn served_declined(stderr: &str) -> Option<(u64, u64)> {
    let line = stderr
        .lines()
        .find(|l| l.contains("JIT Unsafe accessor binds"))?;
    let num = |key: &str| -> Option<u64> {
        let rest = &line[line.find(key)? + key.len()..];
        rest.split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    Some((num("served=")?, num("declined=")?))
}

