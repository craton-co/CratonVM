// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The class-redefinition invalidation PAIRING, and the ratchet that keeps it.
//!
//! `SharedVm.jit` holds two independent stores that both describe a class by
//! its `ClassId`, and a redefinition invalidates BOTH or neither:
//!
//!  * `tiered_manager` — the tiering verdicts (`ineligible`, `c2_bailout`,
//!    trap counts, OSR denials, the de-speculation registry). Cleared by
//!    [`cratonvm_jit::tiered::TieredCompilationManager::on_class_redefined`].
//!  * `profile_store` — the interpreter profile (branch counts, back-edge
//!    counts, per-pc receiver-type tables, call-site counts). Cleared by
//!    `ProfileStore::invalidate_class`.
//!
//! They are siblings, not a hierarchy: `TieredCompilationManager` holds no
//! handle to `ProfileStore` and cannot reach it, so the pairing can only be
//! made by the CALLER. The class-UNLOAD path in `vm/src/memory/gc.rs` has
//! always made it. The class-REDEFINE path did not, and that gap is
//! `docs/feature-designs/real-jdk-mode.md`.
//!
//! # Why a redefinition must purge the profile
//!
//! `ProfileStore` is keyed by `MethodKey { class_id, method_name, descriptor }`
//! and `class_id` is **stable across a redefinition** — that is the whole
//! reason `on_class_redefined` takes it as the identity to reset tiering state
//! *for*. So the same key under which `on_class_redefined` says "the old
//! bytecode's verdicts are worthless" is the key under which the old
//! bytecode's branch counts and per-pc receiver tables keep living, in the
//! same `MethodProfile`, blended with every post-redefinition recording. A
//! conditional-branch bytecode index that meant one thing before the redefine
//! and something structurally different after it — neither bytecode length nor
//! branch targets are required to be stable across a redefine — is recorded
//! and read back as if nothing had happened.
//!
//! It is a QUALITY defect and not a wrong-answer one: every consumer of a
//! profile read pairs it with a runtime guard (the receiver-type consumer
//! re-checks the receiver's actual class id at the guarded call site; the
//! branch-count consumer only influences fallthrough layout). What it costs is
//! the contract `on_class_redefined`'s own doc states — that a redefined
//! method "re-admits at its next stride instead of re-warming from zero" — by
//! letting its profile re-warm from a lie instead of from zero, in a way
//! indistinguishable from the ordinary staleness `jit/src/profile.rs`'s module
//! doc already accepts. That is precisely the risk: nothing points at it.
//!
//! # Why the guard below is textual
//!
//! The fix is one call at each of five production sites. Nothing in either
//! crate's type system makes them travel together — `on_class_redefined` is an
//! ordinary method on an unrelated struct — so the only thing that can notice
//! a sixth call site added without its partner, or an existing pair split by a
//! refactor, is a scan of the source that holds them. [`tests`] does that, and
//! says what it cannot do: it proves the call is WRITTEN next to its partner,
//! never that it RAN.
//!
//! The behavioural halves live where the state does:
//! `jit/tests/r10_tier2_profile_redefinition_pairing.rs` (the two stores are
//! independent, and `ProfileStore::invalidate_class` really does clear a
//! receiver table) and `invalidate_class_sweeps_all_shards` in
//! `jit/src/profile.rs`.

#[cfg(test)]
mod tests;
