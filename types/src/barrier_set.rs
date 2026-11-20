// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Collector-agnostic GC barrier description for JIT codegen.

/// Pre-write barrier mechanism required by the active collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreWriteBarrierKind {
    #[default]
    None,
    Satb,
}

/// Post-write barrier mechanism required by the active collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PostWriteBarrierKind {
    #[default]
    None,
    CardTable,
    G1CardQueue,
}

/// Load barrier mechanism required by the active collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadBarrierKind {
    #[default]
    None,
    ColoredPointer,
}

/// Complete barrier description for one collector configuration.
///
/// Constructed by each collector (`GarbageCollector::barrier_set`) and passed
/// through `DirectHelperTable` so the JIT emits barriers from this description
/// rather than switching on collector names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BarrierSet {
    pub pre_write: PreWriteBarrierKind,
    pub post_write: PostWriteBarrierKind,
    pub load: LoadBarrierKind,
}

impl BarrierSet {
    pub const fn none() -> Self {
        Self {
            pre_write: PreWriteBarrierKind::None,
            post_write: PostWriteBarrierKind::None,
            load: LoadBarrierKind::None,
        }
    }

    pub const fn generational() -> Self {
        Self {
            pre_write: PreWriteBarrierKind::Satb,
            post_write: PostWriteBarrierKind::CardTable,
            load: LoadBarrierKind::None,
        }
    }

    pub const fn g1() -> Self {
        Self {
            pre_write: PreWriteBarrierKind::Satb,
            post_write: PostWriteBarrierKind::G1CardQueue,
            load: LoadBarrierKind::None,
        }
    }

    pub const fn zgc() -> Self {
        Self {
            pre_write: PreWriteBarrierKind::Satb,
            post_write: PostWriteBarrierKind::None,
            load: LoadBarrierKind::ColoredPointer,
        }
    }

    #[inline]
    pub const fn uses_satb_pre_write(self) -> bool {
        matches!(self.pre_write, PreWriteBarrierKind::Satb)
    }

    #[inline]
    pub const fn uses_card_table(self) -> bool {
        matches!(self.post_write, PostWriteBarrierKind::CardTable)
    }

    #[inline]
    pub const fn uses_g1_card_queue(self) -> bool {
        matches!(self.post_write, PostWriteBarrierKind::G1CardQueue)
    }

    #[inline]
    pub const fn uses_colored_load_barrier(self) -> bool {
        matches!(self.load, LoadBarrierKind::ColoredPointer)
    }
}
