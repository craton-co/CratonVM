// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT stack-tracing surface for `Throwable`, `StackWalker`, and continuations.
//!
//! Frame metadata resolution (`bci`, inline chain, oop map) reads the unified
//! [`cratonvm_jit::CompiledSiteRecord`] table on [`cratonvm_jit::CompiledMethod`].

pub(crate) use super::conservative_roots::refresh_moving_young_coverage_for::active_compiled_frames_walk_recorded_opts;
pub use super::conservative_roots::refresh_moving_young_coverage_for::{
    active_compiled_frames, active_compiled_frames_bounded, active_compiled_frames_cut,
    active_compiled_frames_cut_opts, active_compiled_frames_from,
    active_compiled_frames_walk_scoped, active_compiled_frames_whole_opts, ActiveCompiledFrame,
    CompiledWalkOptions, InlinedLevel,
};
