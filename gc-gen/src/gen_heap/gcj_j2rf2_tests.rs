// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gcj j2/rf2 (2026-10-19) — ec47, `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`
//! (default ON, `=0` kills): a Generational major marks the finalizer seeds it
//! used to root (the true-root seed's candidates, a caller's
//! `OldGenFinalizerPass::deferred_seeds`) in the resurrection round, so the OLD
//! children of a dead YOUNG finalizable land in the depth-2 set
//! (`OldGenFinalizerPass::closure`) and their weak references are cleared
//! before `finalize()`, as HotSpot clears them
//! (`docs/feature-designs/generative-gc-architecture.md`).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gcj_j2rf2_`.

use super::*;
