// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Profile-Guided Optimization (PGO) pipeline for the CratonVM JIT compiler.
//!
//! Models runtime profiling data intended to drive:
//! - Branch prediction hints
//! - Receiver-type (virtual call) specialisation
//! - Call-site inlining decisions
//! - Loop unrolling hints
//! - Devirtualisation strategies
//!
//! # ⚠ THIS MODULE IS NOT CONNECTED TO THE RUNNING VM
//!
//! **Nothing outside this file constructs, populates or reads any type
//! declared here.** Verified 2026-07-26: `pgo::`, [`PgoRepository`],
//! [`MethodProfile`], [`CallSiteProfile`], [`ReceiverTypeProfile`] and
//! [`InliningPolicy`] have zero references anywhere in `jit/`, `vm/` or the
//! test suites; `jit/src/lib.rs` declares `pub mod pgo;` and never uses it.
//! Every counter in this module is therefore permanently **zero at runtime**.
//!
//! The consequence for anyone reaching for it: an optimisation gated on
//! [`CallSiteProfile::inline_benefit_score`], [`MethodProfile::get_inline_candidates`],
//! [`ReceiverTypeProfile::is_monomorphic`] or [`InliningPolicy::should_inline`]
//! will silently see "no candidates / not hot / not monomorphic" for every
//! call site in the VM, and its effect will be indistinguishable from being
//! turned off. That failure mode — a capability that reads as landed but never
//! runs — is exactly what `docs/internal/flag-census.md` tracks.
//!
//! **The live profile is [`crate::profile`]**, which the interpreter really
//! does feed (`ProfileStore::record_branch_borrowed` / `record_backedge_borrowed`
//! / `record_receiver_borrowed` / `increment_invocation` from
//! `vm/src/runtime/interpreter.rs`) and which the JIT really does consume
//! (`jit/src/lib.rs` derives `branch_hints` and `loop_unroll_hints` from it and
//! passes them to `x64::compile`). Use `crate::profile::MethodProfile` — note
//! it is a *different* type with the same name — and in particular
//! `crate::profile::MethodProfile::call_site_count`, which reports per-call-site
//! evidence and distinguishes "no data" from "cold".
//!
//! This module is retained as a design sketch for the richer profile the tiered
//! pipeline eventually wants. Do not delete it silently, and do not wire a new
//! optimisation to it without first giving it a recorder.

use std::collections::HashMap;

use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// Branch Profiling
// ---------------------------------------------------------------------------

/// Records how often a conditional branch is taken vs. not-taken at a given BCI.
#[derive(Debug, Clone, Default)]
pub struct BranchProfile {
    pub bytecode_index: u32,
    pub taken_count: u64,
    pub not_taken_count: u64,
}

impl BranchProfile {
    pub fn new(bytecode_index: u32) -> Self {
        Self {
            bytecode_index,
            taken_count: 0,
            not_taken_count: 0,
        }
    }

    /// Total observations.
    pub fn total(&self) -> u64 {
        self.taken_count + self.not_taken_count
    }

    /// Fraction of observations where the branch was taken (0.0 – 1.0).
    pub fn taken_ratio(&self) -> f64 {
        let t = self.total();
        if t == 0 {
            0.5
        } else {
            self.taken_count as f64 / t as f64
        }
    }

    /// Returns `true` when one direction accounts for more than `threshold` of
    /// all observations (e.g. 0.9 means 90 %).
    pub fn is_biased(&self, threshold: f64) -> bool {
        let r = self.taken_ratio();
        r > threshold || r < (1.0 - threshold)
    }

    /// Returns `true` when the branch is taken more than 50 % of the time.
    pub fn likely_taken(&self) -> bool {
        self.taken_ratio() > 0.5
    }

    /// Returns `true` when total observations exceed `min_count`.
    pub fn is_hot(&self, min_count: u64) -> bool {
        self.total() > min_count
    }

    /// Record one observation.
    pub fn update(&mut self, taken: bool) {
        if taken {
            self.taken_count += 1;
        } else {
            self.not_taken_count += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Type Profiling
// ---------------------------------------------------------------------------

/// One entry in a receiver-type profile (a single observed concrete class).
#[derive(Debug, Clone)]
pub struct TypeProfileEntry {
    pub class_id: u32,
    pub method_id: u64,
    pub count: u64,
    /// Fraction of total calls observed at this call site (recomputed on insert).
    pub ratio: f64,
}

/// Records the concrete receiver types observed at a virtual call site.
#[derive(Debug, Clone, Default)]
pub struct ReceiverTypeProfile {
    pub call_site_bci: u32,
    /// At most 8 entries; once full, overflow is tracked via `total_calls` only.
    pub entries: Vec<TypeProfileEntry>,
    pub total_calls: u64,
}

impl ReceiverTypeProfile {
    const MAX_ENTRIES: usize = 8;

    pub fn new(call_site_bci: u32) -> Self {
        Self {
            call_site_bci,
            entries: Vec::new(),
            total_calls: 0,
        }
    }

    /// Record one call with the given concrete receiver class/method ids.
    pub fn add_receiver(&mut self, class_id: u32, method_id: u64) {
        self.total_calls += 1;

        // Update existing entry if present.
        if let Some(e) = self.entries.iter_mut().find(|e| e.class_id == class_id) {
            e.count += 1;
        } else if self.entries.len() < Self::MAX_ENTRIES {
            self.entries.push(TypeProfileEntry {
                class_id,
                method_id,
                count: 1,
                ratio: 0.0,
            });
        }
        // If the table is full and the type is new, we just bump `total_calls`
        // (the entry is not recorded, reflecting megamorphic overflow).

        // Recompute ratios.
        let total = self.total_calls as f64;
        for e in &mut self.entries {
            e.ratio = e.count as f64 / total;
        }
    }

    /// Returns the `class_id` that accounts for >90 % of all observed calls,
    /// if any.
    pub fn dominant_type(&self) -> Option<u32> {
        if self.total_calls == 0 {
            return None;
        }
        self.entries
            .iter()
            .find(|e| e.ratio > 0.9)
            .map(|e| e.class_id)
    }

    /// True iff exactly one concrete type has been observed.
    pub fn is_monomorphic(&self) -> bool {
        self.entries.len() == 1
    }

    /// True iff exactly two concrete types have been observed.
    pub fn is_bimorphic(&self) -> bool {
        self.entries.len() == 2
    }

    /// True iff more than four concrete types have been observed.
    pub fn is_megamorphic(&self) -> bool {
        self.entries.len() > 4
    }

    /// Number of distinct types recorded so far.
    pub fn type_count(&self) -> usize {
        self.entries.len()
    }
}

// ---------------------------------------------------------------------------
// Call-Site Profiling
// ---------------------------------------------------------------------------

/// Records which callees are invoked from a given call site.
/// T10.9.B: FxHashMap — method_id is internal (class_id+method_index packed).
#[derive(Debug, Clone, Default)]
pub struct CallSiteProfile {
    pub bci: u32,
    pub call_count: u64,
    /// method_id → invocation count.
    pub callee_distribution: FxHashMap<u64, u64>,
}

impl CallSiteProfile {
    pub fn new(bci: u32) -> Self {
        Self {
            bci,
            call_count: 0,
            callee_distribution: FxHashMap::default(),
        }
    }

    /// Record one invocation of `callee_id`.
    pub fn record_call(&mut self, callee_id: u64) {
        self.call_count += 1;
        *self.callee_distribution.entry(callee_id).or_insert(0) += 1;
    }

    /// The `method_id` invoked most frequently, if any.
    pub fn most_common_callee(&self) -> Option<u64> {
        self.callee_distribution
            .iter()
            .max_by_key(|(_, &c)| c)
            .map(|(&id, _)| id)
    }

    /// Inline benefit score: (call_count × dominance_ratio) / (callee_size + 1).
    pub fn inline_benefit_score(&self, callee_size: usize) -> f64 {
        if self.call_count == 0 {
            return 0.0;
        }
        let dominance_ratio = match self.most_common_callee() {
            Some(id) => {
                let top = *self.callee_distribution.get(&id).unwrap_or(&0);
                top as f64 / self.call_count as f64
            }
            None => 0.0,
        };
        (self.call_count as f64 * dominance_ratio) / (callee_size + 1) as f64
    }
}

// ---------------------------------------------------------------------------
// Deoptimisation Profiling
// ---------------------------------------------------------------------------

/// Tracks why and how often a method has been deoptimised.
#[derive(Debug, Clone)]
pub struct DeoptProfile {
    pub method_id: u64,
    pub deopt_count: u32,
    pub recompilation_count: u32,
    /// Deopt reason string → count.
    /// T10.9.B: FxHashMap — keys are internal deopt reason strings.
    pub reasons: FxHashMap<String, u32>,
    pub last_deopt_bci: u32,
}

impl DeoptProfile {
    pub fn new(method_id: u64) -> Self {
        Self {
            method_id,
            deopt_count: 0,
            recompilation_count: 0,
            reasons: FxHashMap::default(),
            last_deopt_bci: 0,
        }
    }

    /// True iff the method has been deoptimised fewer than 3 times.
    pub fn is_stable(&self) -> bool {
        self.deopt_count < 3
    }

    /// The most frequently occurring deopt reason, if any have been recorded.
    pub fn dominant_reason(&self) -> Option<&str> {
        self.reasons
            .iter()
            .max_by_key(|(_, &c)| c)
            .map(|(r, _)| r.as_str())
    }

    /// Record a deoptimisation at `bci` with the given `reason`.
    pub fn record_deopt(&mut self, reason: &str, bci: u32) {
        self.deopt_count += 1;
        self.last_deopt_bci = bci;
        *self.reasons.entry(reason.to_owned()).or_insert(0) += 1;
    }
}

// ---------------------------------------------------------------------------
// Loop Profiling
// ---------------------------------------------------------------------------

/// Profiling data for a single back-edge / loop header.
#[derive(Debug, Clone, Default)]
pub struct LoopProfile {
    pub header_bci: u32,
    /// Sampled per-invocation iteration counts.
    pub iteration_counts: Vec<u64>,
    pub backedge_count: u64,
}

impl LoopProfile {
    pub fn new(header_bci: u32) -> Self {
        Self {
            header_bci,
            iteration_counts: Vec::new(),
            backedge_count: 0,
        }
    }

    /// Average trip count over all sampled invocations.
    pub fn avg_iterations(&self) -> f64 {
        if self.iteration_counts.is_empty() {
            return 0.0;
        }
        let sum: u64 = self.iteration_counts.iter().sum();
        sum as f64 / self.iteration_counts.len() as f64
    }

    /// Maximum observed trip count.
    pub fn max_iterations(&self) -> u64 {
        self.iteration_counts.iter().copied().max().unwrap_or(0)
    }

    /// True iff total back-edge executions exceed `threshold`.
    pub fn is_hot(&self, threshold: u64) -> bool {
        self.backedge_count > threshold
    }

    /// Estimated trip count: average if available, otherwise back-edge count.
    pub fn estimated_trip_count(&self) -> u64 {
        if !self.iteration_counts.is_empty() {
            self.avg_iterations() as u64
        } else {
            self.backedge_count
        }
    }

    /// Suggests an unroll factor (2, 4, or 8) when the loop is hot and the
    /// estimated trip count is small enough to make unrolling worthwhile.
    /// Returns `None` when unrolling is not advised.
    pub fn suggests_unrolling(&self, max_unroll: usize) -> Option<usize> {
        if !self.is_hot(100) {
            return None;
        }
        let trip = self.estimated_trip_count();
        // Only unroll when the trip count is known and small.
        if trip == 0 || trip > 128 {
            return None;
        }
        let factor = if trip <= 4 {
            8
        } else if trip <= 16 {
            4
        } else {
            2
        };
        let capped = factor.min(max_unroll);
        if capped >= 2 {
            Some(capped)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Method Profile (aggregated)
// ---------------------------------------------------------------------------

/// Full profiling state for one method.
#[derive(Debug, Clone)]
pub struct MethodProfile {
    pub method_id: u64,
    pub class_name: String,
    pub method_name: String,
    pub descriptor: String,
    pub invocation_count: u64,
    pub backedge_count: u64,
    pub bytecode_size: usize,
    /// 0 = interpreter, 1 = C1, 2 = C1+profiling, 3 = C2.
    pub compilation_level: u8,
    /// BCI → branch profile.
    /// T10.9.B: FxHashMap — bytecode index keys, hot path.
    pub branches: FxHashMap<u32, BranchProfile>,
    /// Call-site BCI → receiver type profile.
    pub type_profiles: FxHashMap<u32, ReceiverTypeProfile>,
    /// Call-site BCI → call-site profile.
    pub call_sites: FxHashMap<u32, CallSiteProfile>,
    /// Loop header BCI → loop profile.
    pub loops: FxHashMap<u32, LoopProfile>,
    pub deopt: DeoptProfile,
}

impl MethodProfile {
    pub fn new(method_id: u64, class_name: &str, method_name: &str, descriptor: &str) -> Self {
        Self {
            method_id,
            class_name: class_name.to_owned(),
            method_name: method_name.to_owned(),
            descriptor: descriptor.to_owned(),
            invocation_count: 0,
            backedge_count: 0,
            bytecode_size: 0,
            compilation_level: 0,
            branches: FxHashMap::default(),
            type_profiles: FxHashMap::default(),
            call_sites: FxHashMap::default(),
            loops: FxHashMap::default(),
            deopt: DeoptProfile::new(method_id),
        }
    }

    /// Record a branch observation at `bci`.
    pub fn record_branch(&mut self, bci: u32, taken: bool) {
        self.branches
            .entry(bci)
            .or_insert_with(|| BranchProfile::new(bci))
            .update(taken);
    }

    /// Record a receiver type observation at a virtual call site.
    pub fn record_receiver(&mut self, bci: u32, class_id: u32, method_id: u64) {
        self.type_profiles
            .entry(bci)
            .or_insert_with(|| ReceiverTypeProfile::new(bci))
            .add_receiver(class_id, method_id);
    }

    /// Record one call from this method to `callee_id` at `bci`.
    pub fn record_call(&mut self, bci: u32, callee_id: u64) {
        self.call_sites
            .entry(bci)
            .or_insert_with(|| CallSiteProfile::new(bci))
            .record_call(callee_id);
    }

    /// Record a back-edge execution at the loop with the given header BCI.
    pub fn record_backedge(&mut self, header_bci: u32) {
        self.backedge_count += 1;
        self.loops
            .entry(header_bci)
            .or_insert_with(|| LoopProfile::new(header_bci))
            .backedge_count += 1;
    }

    /// Return inline candidates derived from call-site profiles, sorted by
    /// descending benefit, limited to `budget` candidates.
    pub fn get_inline_candidates(&self, budget: usize) -> Vec<InlineCandidate> {
        let mut candidates: Vec<InlineCandidate> = self
            .call_sites
            .values()
            .filter_map(|cs| {
                let callee_id = cs.most_common_callee()?;
                let score = cs.inline_benefit_score(0); // size unknown here
                let tp = self.type_profiles.get(&cs.bci);
                Some(InlineCandidate {
                    call_site_bci: cs.bci,
                    callee_method_id: callee_id,
                    callee_class: tp
                        .and_then(|t| t.entries.first())
                        .map(|_| String::new())
                        .unwrap_or_default(),
                    callee_name: String::new(),
                    estimated_benefit: score,
                    callee_size: 0,
                })
            })
            .collect();
        candidates.sort_by(|a, b| {
            b.estimated_benefit
                .partial_cmp(&a.estimated_benefit)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(budget);
        candidates
    }

    /// BCIs of loop headers whose back-edge count exceeds `threshold`.
    pub fn get_hot_loops(&self, threshold: u64) -> Vec<u32> {
        let mut bcis: Vec<u32> = self
            .loops
            .values()
            .filter(|l| l.is_hot(threshold))
            .map(|l| l.header_bci)
            .collect();
        bcis.sort_unstable();
        bcis
    }
}

// ---------------------------------------------------------------------------
// Inlining Budget & Decisions
// ---------------------------------------------------------------------------

/// A candidate call site that the JIT is considering inlining.
#[derive(Debug, Clone)]
pub struct InlineCandidate {
    pub call_site_bci: u32,
    pub callee_method_id: u64,
    pub callee_class: String,
    pub callee_name: String,
    pub estimated_benefit: f64,
    pub callee_size: usize,
}

/// Outcome of an inlining decision.
#[derive(Debug, Clone, PartialEq)]
pub enum InlineDecision {
    /// The call site should be inlined.
    Inline,
    /// The callee is too large to inline.
    TooLarge { size: usize, max: usize },
    /// The call site is not invoked often enough.
    TooRare { count: u64, min: u64 },
    /// The inlining budget for this compilation unit is exhausted.
    BudgetExhausted,
    /// The call site is megamorphic; inlining is not profitable.
    Megamorphic,
}

/// Policy parameters governing inlining decisions.
#[derive(Debug, Clone)]
pub struct InliningPolicy {
    /// Maximum callee bytecode size eligible for inlining (default 35).
    pub max_inline_size: usize,
    /// Maximum total inlined bytecodes per compilation unit (default 250).
    pub max_total_budget: usize,
    /// Minimum call count required for a site to be inlined (default 100).
    pub min_call_count: u64,
    /// Budget multiplier for monomorphic sites (default 2.0).
    pub monomorphic_boost: f64,
}

impl Default for InliningPolicy {
    fn default() -> Self {
        Self {
            max_inline_size: 35,
            max_total_budget: 250,
            min_call_count: 100,
            monomorphic_boost: 2.0,
        }
    }
}

impl InliningPolicy {
    /// Decide whether to inline `candidate` given the method's profile.
    /// `profile` is used to check type-profile morphism at the call site.
    pub fn should_inline(
        &self,
        candidate: &InlineCandidate,
        profile: &MethodProfile,
    ) -> InlineDecision {
        // Check for megamorphic call site.
        if let Some(tp) = profile.type_profiles.get(&candidate.call_site_bci) {
            if tp.is_megamorphic() {
                return InlineDecision::Megamorphic;
            }
        }

        // Determine effective size limit (boost monomorphic sites).
        let effective_max = if profile
            .type_profiles
            .get(&candidate.call_site_bci)
            .map(|t| t.is_monomorphic())
            .unwrap_or(false)
        {
            (self.max_inline_size as f64 * self.monomorphic_boost) as usize
        } else {
            self.max_inline_size
        };

        if candidate.callee_size > effective_max {
            return InlineDecision::TooLarge {
                size: candidate.callee_size,
                max: effective_max,
            };
        }

        // Check call frequency.
        let call_count = profile
            .call_sites
            .get(&candidate.call_site_bci)
            .map(|cs| cs.call_count)
            .unwrap_or(0);
        if call_count < self.min_call_count {
            return InlineDecision::TooRare {
                count: call_count,
                min: self.min_call_count,
            };
        }

        InlineDecision::Inline
    }

    /// Sort candidates descending by estimated benefit (in-place).
    pub fn rank_candidates(&self, candidates: &mut Vec<InlineCandidate>) {
        candidates.sort_by(|a, b| {
            b.estimated_benefit
                .partial_cmp(&a.estimated_benefit)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}

// ---------------------------------------------------------------------------
// PGO Repository
// ---------------------------------------------------------------------------

/// Central store for all collected method profiles.
/// T10.9.B: FxHashMap — method_id is internal packed key.
#[derive(Debug, Clone, Default)]
pub struct PgoRepository {
    /// method_id → profile.
    pub profiles: FxHashMap<u64, MethodProfile>,
    /// Total number of methods ever tracked.
    pub total_methods: usize,
    /// Number of hot methods (invocation_count > 10 000).
    pub hot_methods: usize,
}

impl PgoRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a mutable reference to the profile for `method_id`, creating one
    /// if it does not yet exist.
    pub fn get_or_create(
        &mut self,
        method_id: u64,
        class: &str,
        method: &str,
        desc: &str,
    ) -> &mut MethodProfile {
        if !self.profiles.contains_key(&method_id) {
            let p = MethodProfile::new(method_id, class, method, desc);
            self.profiles.insert(method_id, p);
            self.total_methods += 1;
        }
        self.profiles.get_mut(&method_id).unwrap()
    }

    /// Immutable access to a profile by `method_id`.
    pub fn get(&self, method_id: u64) -> Option<&MethodProfile> {
        self.profiles.get(&method_id)
    }

    /// All methods whose invocation count exceeds `threshold`.
    pub fn get_hot_methods(&self, threshold: u64) -> Vec<&MethodProfile> {
        let mut hot: Vec<&MethodProfile> = self
            .profiles
            .values()
            .filter(|p| p.invocation_count > threshold)
            .collect();
        hot.sort_by_key(|p| std::cmp::Reverse(p.invocation_count));
        hot
    }

    /// Build a summary snapshot of the repository.
    pub fn serialize_summary(&self) -> PgoSummary {
        let hot_threshold = 10_000u64;
        let mut total_branches = 0usize;
        let mut biased_branches = 0usize;
        let mut total_call_sites = 0usize;
        let mut monomorphic_sites = 0usize;
        let mut bimorphic_sites = 0usize;
        let mut megamorphic_sites = 0usize;
        let mut total_loops = 0usize;
        let mut hot_loops = 0usize;

        for p in self.profiles.values() {
            total_branches += p.branches.len();
            biased_branches += p.branches.values().filter(|b| b.is_biased(0.9)).count();
            total_call_sites += p.type_profiles.len();
            monomorphic_sites += p
                .type_profiles
                .values()
                .filter(|t| t.is_monomorphic())
                .count();
            bimorphic_sites += p
                .type_profiles
                .values()
                .filter(|t| t.is_bimorphic())
                .count();
            megamorphic_sites += p
                .type_profiles
                .values()
                .filter(|t| t.is_megamorphic())
                .count();
            total_loops += p.loops.len();
            hot_loops += p.loops.values().filter(|l| l.is_hot(1000)).count();
        }

        let hot_methods = self
            .profiles
            .values()
            .filter(|p| p.invocation_count > hot_threshold)
            .count();

        PgoSummary {
            total_methods: self.total_methods,
            hot_methods,
            total_branches,
            biased_branches,
            total_call_sites,
            monomorphic_sites,
            bimorphic_sites,
            megamorphic_sites,
            total_loops,
            hot_loops,
        }
    }

    /// Merge another repository into this one, accumulating all counts.
    pub fn merge(&mut self, other: PgoRepository) {
        for (id, other_profile) in other.profiles {
            if let Some(mine) = self.profiles.get_mut(&id) {
                mine.invocation_count += other_profile.invocation_count;
                mine.backedge_count += other_profile.backedge_count;
                // Merge branches.
                for (bci, ob) in other_profile.branches {
                    let b = mine
                        .branches
                        .entry(bci)
                        .or_insert_with(|| BranchProfile::new(bci));
                    b.taken_count += ob.taken_count;
                    b.not_taken_count += ob.not_taken_count;
                }
                // Merge type profiles.
                for (bci, otp) in other_profile.type_profiles {
                    let tp = mine
                        .type_profiles
                        .entry(bci)
                        .or_insert_with(|| ReceiverTypeProfile::new(bci));
                    tp.total_calls += otp.total_calls;
                    for oe in &otp.entries {
                        if let Some(e) = tp.entries.iter_mut().find(|e| e.class_id == oe.class_id) {
                            e.count += oe.count;
                        } else if tp.entries.len() < ReceiverTypeProfile::MAX_ENTRIES {
                            tp.entries.push(oe.clone());
                        }
                    }
                    let total = tp.total_calls as f64;
                    for e in &mut tp.entries {
                        e.ratio = e.count as f64 / total;
                    }
                }
                // Merge call sites.
                for (bci, ocs) in other_profile.call_sites {
                    let cs = mine
                        .call_sites
                        .entry(bci)
                        .or_insert_with(|| CallSiteProfile::new(bci));
                    cs.call_count += ocs.call_count;
                    for (callee, cnt) in ocs.callee_distribution {
                        *cs.callee_distribution.entry(callee).or_insert(0) += cnt;
                    }
                }
                // Merge loops.
                for (bci, ol) in other_profile.loops {
                    let lp = mine
                        .loops
                        .entry(bci)
                        .or_insert_with(|| LoopProfile::new(bci));
                    lp.backedge_count += ol.backedge_count;
                    lp.iteration_counts.extend_from_slice(&ol.iteration_counts);
                }
                // Merge deopt.
                mine.deopt.deopt_count += other_profile.deopt.deopt_count;
                mine.deopt.recompilation_count += other_profile.deopt.recompilation_count;
                for (reason, cnt) in other_profile.deopt.reasons {
                    *mine.deopt.reasons.entry(reason).or_insert(0) += cnt;
                }
            } else {
                self.profiles.insert(id, other_profile);
                self.total_methods += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PGO Summary
// ---------------------------------------------------------------------------

/// Lightweight snapshot of repository statistics.
#[derive(Debug, Clone, Default)]
pub struct PgoSummary {
    pub total_methods: usize,
    pub hot_methods: usize,
    pub total_branches: usize,
    /// Branches whose taken ratio exceeds 90 % in one direction.
    pub biased_branches: usize,
    pub total_call_sites: usize,
    pub monomorphic_sites: usize,
    pub bimorphic_sites: usize,
    pub megamorphic_sites: usize,
    pub total_loops: usize,
    pub hot_loops: usize,
}

// ---------------------------------------------------------------------------
// Profile-Driven Devirtualisation
// ---------------------------------------------------------------------------

/// Strategy chosen for a virtual call site.
#[derive(Debug, Clone, PartialEq)]
pub enum DevirtStrategy {
    /// Inline the dominant callee (method_id).
    Inline(u64),
    /// Emit a type guard + direct call to the dominant callee.
    DirectCall(u64),
    /// Bimorphic: emit an if/else selecting between two callees.
    IfThenElse(u64, u64),
    /// No profitable specialisation; fall back to vtable dispatch.
    Megamorphic,
}

/// A devirtualisation decision for one call site.
#[derive(Debug, Clone)]
pub struct DevirtDecision {
    pub call_site_bci: u32,
    pub strategy: DevirtStrategy,
    /// Confidence in the strategy (0.0 – 1.0).
    pub confidence: f64,
}

/// Analyses a method's type profiles and produces devirtualisation decisions.
pub struct DevirtualizationAnalyzer;

impl DevirtualizationAnalyzer {
    /// Analyse all virtual call sites in `profile` and return decisions.
    pub fn analyze(&self, profile: &MethodProfile, policy: &InliningPolicy) -> Vec<DevirtDecision> {
        let mut decisions = Vec::new();

        for (bci, tp) in &profile.type_profiles {
            if tp.total_calls == 0 {
                continue;
            }

            let strategy = if tp.is_megamorphic() {
                DevirtDecision {
                    call_site_bci: *bci,
                    strategy: DevirtStrategy::Megamorphic,
                    confidence: 1.0,
                }
            } else if tp.is_monomorphic() {
                let entry = &tp.entries[0];
                let confidence = entry.ratio;
                // Prefer inlining if the callee is small enough and call count high enough.
                let cs_count = profile
                    .call_sites
                    .get(bci)
                    .map(|cs| cs.call_count)
                    .unwrap_or(0);
                let strategy = if cs_count >= policy.min_call_count {
                    DevirtStrategy::Inline(entry.method_id)
                } else {
                    DevirtStrategy::DirectCall(entry.method_id)
                };
                DevirtDecision {
                    call_site_bci: *bci,
                    strategy,
                    confidence,
                }
            } else if tp.is_bimorphic() {
                let a = &tp.entries[0];
                let b = &tp.entries[1];
                let confidence = a.ratio + b.ratio;
                DevirtDecision {
                    call_site_bci: *bci,
                    strategy: DevirtStrategy::IfThenElse(a.method_id, b.method_id),
                    confidence,
                }
            } else {
                // Polymorphic (3 or 4 types): use direct call to dominant if possible.
                if let Some(dominant_class) = tp.dominant_type() {
                    let entry = tp
                        .entries
                        .iter()
                        .find(|e| e.class_id == dominant_class)
                        .unwrap();
                    DevirtDecision {
                        call_site_bci: *bci,
                        strategy: DevirtStrategy::DirectCall(entry.method_id),
                        confidence: entry.ratio,
                    }
                } else {
                    DevirtDecision {
                        call_site_bci: *bci,
                        strategy: DevirtStrategy::Megamorphic,
                        confidence: 1.0,
                    }
                }
            };

            decisions.push(strategy);
        }

        decisions.sort_by_key(|d| d.call_site_bci);
        decisions
    }
}

// ---------------------------------------------------------------------------
// Profile Serialisation (AOT integration)
// ---------------------------------------------------------------------------

/// Magic marker written at the start of every serialised PGO blob.
const PGO_MAGIC: u32 = 0xAB1E_ED01;
/// Current format version.
const PGO_VERSION: u16 = 1;

/// Serialises and deserialises `PgoRepository` blobs for AOT integration.
pub struct ProfileSerializer;

impl ProfileSerializer {
    /// Serialise `repo` into a compact binary blob.
    ///
    /// Format:
    /// ```text
    /// [u32 magic] [u16 version] [u32 method_count]
    /// per method:
    ///   [u64 method_id]
    ///   [u64 invocation_count]
    ///   [u64 backedge_count]
    ///   [u32 branch_count] per branch: [u32 bci][u64 taken][u64 not_taken]
    ///   [u32 type_profile_count] per tp: [u32 bci][u64 total][u32 entry_count]
    ///     per entry: [u32 class_id][u64 method_id][u64 count]
    ///   [u32 call_site_count] per cs: [u32 bci][u64 call_count][u32 callee_count]
    ///     per callee: [u64 callee_id][u64 count]
    ///   [u32 loop_count] per loop: [u32 header_bci][u64 backedge_count]
    ///   [u32 deopt_count][u32 recomp_count][u32 last_deopt_bci]
    ///   [u32 reason_count] per reason: [u32 len][bytes][u32 count]
    ///   [u32 class_name_len][bytes]
    ///   [u32 method_name_len][bytes]
    ///   [u32 descriptor_len][bytes]
    /// ```
    pub fn serialize(repo: &PgoRepository) -> Vec<u8> {
        let mut buf = Vec::new();
        // Header.
        buf.extend_from_slice(&PGO_MAGIC.to_le_bytes());
        buf.extend_from_slice(&PGO_VERSION.to_le_bytes());
        buf.extend_from_slice(&(repo.profiles.len() as u32).to_le_bytes());

        for p in repo.profiles.values() {
            buf.extend_from_slice(&p.method_id.to_le_bytes());
            buf.extend_from_slice(&p.invocation_count.to_le_bytes());
            buf.extend_from_slice(&p.backedge_count.to_le_bytes());

            // Branches.
            buf.extend_from_slice(&(p.branches.len() as u32).to_le_bytes());
            for (bci, b) in &p.branches {
                buf.extend_from_slice(&bci.to_le_bytes());
                buf.extend_from_slice(&b.taken_count.to_le_bytes());
                buf.extend_from_slice(&b.not_taken_count.to_le_bytes());
            }

            // Type profiles.
            buf.extend_from_slice(&(p.type_profiles.len() as u32).to_le_bytes());
            for (bci, tp) in &p.type_profiles {
                buf.extend_from_slice(&bci.to_le_bytes());
                buf.extend_from_slice(&tp.total_calls.to_le_bytes());
                buf.extend_from_slice(&(tp.entries.len() as u32).to_le_bytes());
                for e in &tp.entries {
                    buf.extend_from_slice(&e.class_id.to_le_bytes());
                    buf.extend_from_slice(&e.method_id.to_le_bytes());
                    buf.extend_from_slice(&e.count.to_le_bytes());
                }
            }

            // Call sites.
            buf.extend_from_slice(&(p.call_sites.len() as u32).to_le_bytes());
            for (bci, cs) in &p.call_sites {
                buf.extend_from_slice(&bci.to_le_bytes());
                buf.extend_from_slice(&cs.call_count.to_le_bytes());
                buf.extend_from_slice(&(cs.callee_distribution.len() as u32).to_le_bytes());
                for (callee_id, cnt) in &cs.callee_distribution {
                    buf.extend_from_slice(&callee_id.to_le_bytes());
                    buf.extend_from_slice(&cnt.to_le_bytes());
                }
            }

            // Loops.
            buf.extend_from_slice(&(p.loops.len() as u32).to_le_bytes());
            for (hbci, lp) in &p.loops {
                buf.extend_from_slice(&hbci.to_le_bytes());
                buf.extend_from_slice(&lp.backedge_count.to_le_bytes());
            }

            // Deopt.
            buf.extend_from_slice(&p.deopt.deopt_count.to_le_bytes());
            buf.extend_from_slice(&p.deopt.recompilation_count.to_le_bytes());
            buf.extend_from_slice(&p.deopt.last_deopt_bci.to_le_bytes());
            buf.extend_from_slice(&(p.deopt.reasons.len() as u32).to_le_bytes());
            for (reason, cnt) in &p.deopt.reasons {
                let rb = reason.as_bytes();
                buf.extend_from_slice(&(rb.len() as u32).to_le_bytes());
                buf.extend_from_slice(rb);
                buf.extend_from_slice(&cnt.to_le_bytes());
            }

            // String fields.
            fn write_str(buf: &mut Vec<u8>, s: &str) {
                let b = s.as_bytes();
                buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                buf.extend_from_slice(b);
            }
            write_str(&mut buf, &p.class_name);
            write_str(&mut buf, &p.method_name);
            write_str(&mut buf, &p.descriptor);
        }
        buf
    }

    /// Deserialise a blob produced by [`serialize`].
    pub fn deserialize(data: &[u8]) -> Result<PgoRepository, String> {
        let mut pos = 0usize;

        macro_rules! read_u16 {
            () => {{
                if pos + 2 > data.len() {
                    return Err("unexpected EOF (u16)".into());
                }
                let v = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap());
                pos += 2;
                v
            }};
        }
        macro_rules! read_u32 {
            () => {{
                if pos + 4 > data.len() {
                    return Err("unexpected EOF (u32)".into());
                }
                let v = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
                pos += 4;
                v
            }};
        }
        macro_rules! read_u64 {
            () => {{
                if pos + 8 > data.len() {
                    return Err("unexpected EOF (u64)".into());
                }
                let v = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                v
            }};
        }
        macro_rules! read_str {
            () => {{
                let len = read_u32!() as usize;
                if pos + len > data.len() {
                    return Err("unexpected EOF (string)".into());
                }
                let s = std::str::from_utf8(&data[pos..pos + len])
                    .map_err(|e| format!("utf8 error: {e}"))?
                    .to_owned();
                pos += len;
                s
            }};
        }

        let magic = read_u32!();
        if magic != PGO_MAGIC {
            return Err(format!("bad magic: 0x{magic:08X}"));
        }
        let version = read_u16!();
        if version != PGO_VERSION {
            return Err(format!("unsupported version: {version}"));
        }
        let method_count = read_u32!() as usize;

        let mut repo = PgoRepository::new();

        for _ in 0..method_count {
            let method_id = read_u64!();
            let inv_count = read_u64!();
            let back_count = read_u64!();

            // Branches.
            let n_branches = read_u32!() as usize;
            let mut branches: FxHashMap<u32, BranchProfile> = FxHashMap::default();
            for _ in 0..n_branches {
                let bci = read_u32!();
                let taken = read_u64!();
                let not_taken = read_u64!();
                branches.insert(
                    bci,
                    BranchProfile {
                        bytecode_index: bci,
                        taken_count: taken,
                        not_taken_count: not_taken,
                    },
                );
            }

            // Type profiles.
            let n_tp = read_u32!() as usize;
            let mut type_profiles: FxHashMap<u32, ReceiverTypeProfile> = FxHashMap::default();
            for _ in 0..n_tp {
                let bci = read_u32!();
                let total_calls = read_u64!();
                let n_entries = read_u32!() as usize;
                let mut entries = Vec::with_capacity(n_entries);
                for _ in 0..n_entries {
                    let class_id = read_u32!();
                    let m_id = read_u64!();
                    let count = read_u64!();
                    let ratio = if total_calls > 0 {
                        count as f64 / total_calls as f64
                    } else {
                        0.0
                    };
                    entries.push(TypeProfileEntry {
                        class_id,
                        method_id: m_id,
                        count,
                        ratio,
                    });
                }
                type_profiles.insert(
                    bci,
                    ReceiverTypeProfile {
                        call_site_bci: bci,
                        entries,
                        total_calls,
                    },
                );
            }

            // Call sites.
            let n_cs = read_u32!() as usize;
            let mut call_sites: FxHashMap<u32, CallSiteProfile> = FxHashMap::default();
            for _ in 0..n_cs {
                let bci = read_u32!();
                let call_count = read_u64!();
                let n_callees = read_u32!() as usize;
                let mut dist = FxHashMap::default();
                for _ in 0..n_callees {
                    let callee_id = read_u64!();
                    let cnt = read_u64!();
                    dist.insert(callee_id, cnt);
                }
                call_sites.insert(
                    bci,
                    CallSiteProfile {
                        bci,
                        call_count,
                        callee_distribution: dist,
                    },
                );
            }

            // Loops.
            let n_loops = read_u32!() as usize;
            let mut loops = FxHashMap::default();
            for _ in 0..n_loops {
                let hbci = read_u32!();
                let be = read_u64!();
                loops.insert(
                    hbci,
                    LoopProfile {
                        header_bci: hbci,
                        iteration_counts: Vec::new(),
                        backedge_count: be,
                    },
                );
            }

            // Deopt.
            let deopt_count = read_u32!();
            let recomp_count = read_u32!();
            let last_bci = read_u32!();
            let n_reasons = read_u32!() as usize;
            let mut reasons = FxHashMap::default();
            for _ in 0..n_reasons {
                let reason = read_str!();
                let cnt = read_u32!();
                reasons.insert(reason, cnt);
            }

            let class_name = read_str!();
            let method_name = read_str!();
            let descriptor = read_str!();

            let profile = MethodProfile {
                method_id,
                class_name,
                method_name,
                descriptor,
                invocation_count: inv_count,
                backedge_count: back_count,
                bytecode_size: 0,
                compilation_level: 0,
                branches,
                type_profiles,
                call_sites,
                loops,
                deopt: DeoptProfile {
                    method_id,
                    deopt_count,
                    recompilation_count: recomp_count,
                    reasons,
                    last_deopt_bci: last_bci,
                },
            };
            repo.profiles.insert(method_id, profile);
            repo.total_methods += 1;
        }

        Ok(repo)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- BranchProfile -------------------------------------------------------

    #[test]
    fn branch_total_empty() {
        let bp = BranchProfile::new(0);
        assert_eq!(bp.total(), 0);
    }

    #[test]
    fn branch_taken_ratio_empty_is_half() {
        let bp = BranchProfile::new(0);
        assert!((bp.taken_ratio() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn branch_update_taken() {
        let mut bp = BranchProfile::new(10);
        bp.update(true);
        bp.update(true);
        bp.update(false);
        assert_eq!(bp.total(), 3);
        assert_eq!(bp.taken_count, 2);
        assert_eq!(bp.not_taken_count, 1);
    }

    #[test]
    fn branch_taken_ratio() {
        let mut bp = BranchProfile::new(0);
        for _ in 0..9 {
            bp.update(true);
        }
        bp.update(false);
        assert!((bp.taken_ratio() - 0.9).abs() < 1e-9);
    }

    #[test]
    fn branch_is_biased_true() {
        let mut bp = BranchProfile::new(0);
        for _ in 0..95 {
            bp.update(true);
        }
        for _ in 0..5 {
            bp.update(false);
        }
        assert!(bp.is_biased(0.9));
    }

    #[test]
    fn branch_is_biased_false() {
        let mut bp = BranchProfile::new(0);
        for _ in 0..55 {
            bp.update(true);
        }
        for _ in 0..45 {
            bp.update(false);
        }
        assert!(!bp.is_biased(0.9));
    }

    #[test]
    fn branch_not_taken_biased() {
        let mut bp = BranchProfile::new(0);
        for _ in 0..5 {
            bp.update(true);
        }
        for _ in 0..95 {
            bp.update(false);
        }
        assert!(bp.is_biased(0.9));
        assert!(!bp.likely_taken());
    }

    #[test]
    fn branch_likely_taken() {
        let mut bp = BranchProfile::new(0);
        for _ in 0..6 {
            bp.update(true);
        }
        for _ in 0..4 {
            bp.update(false);
        }
        assert!(bp.likely_taken());
    }

    #[test]
    fn branch_is_hot() {
        let mut bp = BranchProfile::new(0);
        for _ in 0..101 {
            bp.update(true);
        }
        assert!(bp.is_hot(100));
        assert!(!bp.is_hot(200));
    }

    // ---- ReceiverTypeProfile ------------------------------------------------

    #[test]
    fn type_profile_empty_is_not_mono() {
        let tp = ReceiverTypeProfile::new(5);
        assert!(!tp.is_monomorphic());
        assert_eq!(tp.type_count(), 0);
        assert!(tp.dominant_type().is_none());
    }

    #[test]
    fn type_profile_monomorphic() {
        let mut tp = ReceiverTypeProfile::new(0);
        for _ in 0..10 {
            tp.add_receiver(42, 1001);
        }
        assert!(tp.is_monomorphic());
        assert!(!tp.is_bimorphic());
        assert!(!tp.is_megamorphic());
        assert_eq!(tp.dominant_type(), Some(42));
    }

    #[test]
    fn type_profile_bimorphic() {
        let mut tp = ReceiverTypeProfile::new(0);
        for _ in 0..5 {
            tp.add_receiver(1, 100);
        }
        for _ in 0..5 {
            tp.add_receiver(2, 200);
        }
        assert!(tp.is_bimorphic());
        assert!(!tp.is_monomorphic());
        assert!(!tp.is_megamorphic());
    }

    #[test]
    fn type_profile_megamorphic() {
        let mut tp = ReceiverTypeProfile::new(0);
        for i in 0..5 {
            tp.add_receiver(i, i as u64 * 100);
        }
        assert!(tp.is_megamorphic());
    }

    #[test]
    fn type_profile_dominant_90_percent() {
        let mut tp = ReceiverTypeProfile::new(0);
        for _ in 0..91 {
            tp.add_receiver(1, 100);
        }
        for _ in 0..9 {
            tp.add_receiver(2, 200);
        }
        assert_eq!(tp.dominant_type(), Some(1));
    }

    #[test]
    fn type_profile_no_dominant_under_90() {
        let mut tp = ReceiverTypeProfile::new(0);
        for _ in 0..85 {
            tp.add_receiver(1, 100);
        }
        for _ in 0..15 {
            tp.add_receiver(2, 200);
        }
        assert_eq!(tp.dominant_type(), None);
    }

    #[test]
    fn type_profile_max_entries_capped_at_8() {
        let mut tp = ReceiverTypeProfile::new(0);
        for i in 0..12u32 {
            tp.add_receiver(i, i as u64);
        }
        assert!(tp.entries.len() <= 8);
        assert!(tp.total_calls >= 12);
    }

    #[test]
    fn type_profile_ratios_sum_to_at_most_one() {
        let mut tp = ReceiverTypeProfile::new(0);
        for i in 0..4u32 {
            for _ in 0..25 {
                tp.add_receiver(i, i as u64);
            }
        }
        let sum: f64 = tp.entries.iter().map(|e| e.ratio).sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }

    // ---- CallSiteProfile ----------------------------------------------------

    #[test]
    fn call_site_most_common_callee() {
        let mut cs = CallSiteProfile::new(20);
        cs.record_call(10);
        cs.record_call(10);
        cs.record_call(20);
        assert_eq!(cs.most_common_callee(), Some(10));
    }

    #[test]
    fn call_site_benefit_score_empty() {
        let cs = CallSiteProfile::new(0);
        assert_eq!(cs.inline_benefit_score(10), 0.0);
    }

    #[test]
    fn call_site_benefit_score_single_callee() {
        let mut cs = CallSiteProfile::new(0);
        for _ in 0..100 {
            cs.record_call(42);
        }
        // dominance_ratio = 1.0, callee_size = 9 => score = 100 / 10
        let score = cs.inline_benefit_score(9);
        assert!((score - 10.0).abs() < 1e-9);
    }

    #[test]
    fn call_site_benefit_score_split_callee() {
        let mut cs = CallSiteProfile::new(0);
        for _ in 0..50 {
            cs.record_call(1);
        }
        for _ in 0..50 {
            cs.record_call(2);
        }
        // dominance_ratio = 0.5, callee_size = 9 => score = 100 * 0.5 / 10 = 5.0
        let score = cs.inline_benefit_score(9);
        assert!((score - 5.0).abs() < 1e-9);
    }

    // ---- DeoptProfile -------------------------------------------------------

    #[test]
    fn deopt_is_stable_initially() {
        let d = DeoptProfile::new(1);
        assert!(d.is_stable());
    }

    #[test]
    fn deopt_becomes_unstable_after_3() {
        let mut d = DeoptProfile::new(1);
        d.record_deopt("null_check", 10);
        d.record_deopt("type_check", 20);
        d.record_deopt("overflow", 30);
        assert!(!d.is_stable());
    }

    #[test]
    fn deopt_dominant_reason() {
        let mut d = DeoptProfile::new(1);
        d.record_deopt("type_check", 5);
        d.record_deopt("type_check", 6);
        d.record_deopt("null_check", 7);
        assert_eq!(d.dominant_reason(), Some("type_check"));
    }

    #[test]
    fn deopt_last_bci_updated() {
        let mut d = DeoptProfile::new(1);
        d.record_deopt("r", 99);
        assert_eq!(d.last_deopt_bci, 99);
    }

    // ---- LoopProfile --------------------------------------------------------

    #[test]
    fn loop_avg_iterations_empty() {
        let lp = LoopProfile::new(0);
        assert_eq!(lp.avg_iterations(), 0.0);
    }

    #[test]
    fn loop_avg_iterations() {
        let mut lp = LoopProfile::new(0);
        lp.iteration_counts = vec![4, 6, 10];
        assert!((lp.avg_iterations() - (20.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn loop_max_iterations() {
        let mut lp = LoopProfile::new(0);
        lp.iteration_counts = vec![3, 99, 50];
        assert_eq!(lp.max_iterations(), 99);
    }

    #[test]
    fn loop_is_hot() {
        let mut lp = LoopProfile::new(0);
        lp.backedge_count = 5000;
        assert!(lp.is_hot(1000));
        assert!(!lp.is_hot(5000));
    }

    #[test]
    fn loop_estimated_trip_count_uses_avg_when_available() {
        let mut lp = LoopProfile::new(0);
        lp.backedge_count = 9999;
        lp.iteration_counts = vec![8, 8];
        assert_eq!(lp.estimated_trip_count(), 8);
    }

    #[test]
    fn loop_suggests_unrolling_hot_small() {
        let mut lp = LoopProfile::new(0);
        lp.backedge_count = 10_000;
        lp.iteration_counts = vec![4, 4, 4];
        // trip count ~ 4 => factor 8, capped to max_unroll=8
        let factor = lp.suggests_unrolling(8);
        assert!(factor.is_some());
        assert!(factor.unwrap() >= 2);
    }

    #[test]
    fn loop_no_unrolling_cold() {
        let mut lp = LoopProfile::new(0);
        lp.backedge_count = 10;
        lp.iteration_counts = vec![4];
        assert_eq!(lp.suggests_unrolling(8), None);
    }

    #[test]
    fn loop_no_unrolling_large_trip() {
        let mut lp = LoopProfile::new(0);
        lp.backedge_count = 50_000;
        lp.iteration_counts = vec![200, 300];
        assert_eq!(lp.suggests_unrolling(8), None);
    }

    // ---- MethodProfile ------------------------------------------------------

    #[test]
    fn method_profile_record_branch() {
        let mut mp = MethodProfile::new(1, "Foo", "bar", "()V");
        mp.record_branch(10, true);
        mp.record_branch(10, false);
        let b = mp.branches.get(&10).unwrap();
        assert_eq!(b.total(), 2);
    }

    #[test]
    fn method_profile_record_receiver() {
        let mut mp = MethodProfile::new(1, "Foo", "bar", "()V");
        mp.record_receiver(20, 99, 1001);
        let tp = mp.type_profiles.get(&20).unwrap();
        assert!(tp.is_monomorphic());
    }

    #[test]
    fn method_profile_record_call() {
        let mut mp = MethodProfile::new(1, "Foo", "bar", "()V");
        for _ in 0..200 {
            mp.record_call(30, 55);
        }
        let cs = mp.call_sites.get(&30).unwrap();
        assert_eq!(cs.call_count, 200);
    }

    #[test]
    fn method_profile_record_backedge() {
        let mut mp = MethodProfile::new(1, "Foo", "bar", "()V");
        mp.record_backedge(40);
        mp.record_backedge(40);
        assert_eq!(mp.backedge_count, 2);
        assert_eq!(mp.loops[&40].backedge_count, 2);
    }

    #[test]
    fn method_profile_get_hot_loops() {
        let mut mp = MethodProfile::new(1, "Foo", "bar", "()V");
        for _ in 0..5000 {
            mp.record_backedge(10);
        }
        mp.record_backedge(20); // cold
        let hot = mp.get_hot_loops(100);
        assert!(hot.contains(&10));
        assert!(!hot.contains(&20));
    }

    #[test]
    fn method_profile_get_inline_candidates() {
        let mut mp = MethodProfile::new(1, "Foo", "bar", "()V");
        for _ in 0..500 {
            mp.record_call(5, 99);
        }
        let cands = mp.get_inline_candidates(10);
        assert!(!cands.is_empty());
        assert_eq!(cands[0].call_site_bci, 5);
    }

    // ---- InliningPolicy -----------------------------------------------------

    #[test]
    fn inlining_policy_defaults() {
        let p = InliningPolicy::default();
        assert_eq!(p.max_inline_size, 35);
        assert_eq!(p.max_total_budget, 250);
        assert_eq!(p.min_call_count, 100);
    }

    #[test]
    fn inlining_policy_too_rare() {
        let policy = InliningPolicy::default();
        let mut mp = MethodProfile::new(1, "A", "m", "()V");
        for _ in 0..50 {
            mp.record_call(0, 1);
        } // only 50, need 100
        let cand = InlineCandidate {
            call_site_bci: 0,
            callee_method_id: 1,
            callee_class: "B".into(),
            callee_name: "n".into(),
            estimated_benefit: 1.0,
            callee_size: 10,
        };
        assert_eq!(
            policy.should_inline(&cand, &mp),
            InlineDecision::TooRare {
                count: 50,
                min: 100
            }
        );
    }

    #[test]
    fn inlining_policy_too_large() {
        let policy = InliningPolicy::default();
        let mut mp = MethodProfile::new(1, "A", "m", "()V");
        for _ in 0..200 {
            mp.record_call(0, 1);
        }
        let cand = InlineCandidate {
            call_site_bci: 0,
            callee_method_id: 1,
            callee_class: "B".into(),
            callee_name: "n".into(),
            estimated_benefit: 100.0,
            callee_size: 50, // > 35
        };
        match policy.should_inline(&cand, &mp) {
            InlineDecision::TooLarge { size, max } => {
                assert_eq!(size, 50);
                assert_eq!(max, 35);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn inlining_policy_monomorphic_boost_allows_larger() {
        let policy = InliningPolicy::default(); // boost = 2.0, max = 35 => effective 70
        let mut mp = MethodProfile::new(1, "A", "m", "()V");
        for _ in 0..200 {
            mp.record_call(0, 1);
        }
        mp.record_receiver(0, 7, 1); // monomorphic
        let cand = InlineCandidate {
            call_site_bci: 0,
            callee_method_id: 1,
            callee_class: "B".into(),
            callee_name: "n".into(),
            estimated_benefit: 100.0,
            callee_size: 60, // > 35 but < 70
        };
        assert_eq!(policy.should_inline(&cand, &mp), InlineDecision::Inline);
    }

    #[test]
    fn inlining_policy_megamorphic_rejected() {
        let policy = InliningPolicy::default();
        let mut mp = MethodProfile::new(1, "A", "m", "()V");
        for _ in 0..500 {
            mp.record_call(0, 1);
        }
        for i in 0..5u32 {
            mp.record_receiver(0, i, i as u64);
        }
        let cand = InlineCandidate {
            call_site_bci: 0,
            callee_method_id: 1,
            callee_class: "B".into(),
            callee_name: "n".into(),
            estimated_benefit: 100.0,
            callee_size: 10,
        };
        assert_eq!(
            policy.should_inline(&cand, &mp),
            InlineDecision::Megamorphic
        );
    }

    #[test]
    fn inlining_policy_rank_candidates() {
        let policy = InliningPolicy::default();
        let mut cands = vec![
            InlineCandidate {
                call_site_bci: 0,
                callee_method_id: 1,
                callee_class: "".into(),
                callee_name: "".into(),
                estimated_benefit: 3.0,
                callee_size: 10,
            },
            InlineCandidate {
                call_site_bci: 1,
                callee_method_id: 2,
                callee_class: "".into(),
                callee_name: "".into(),
                estimated_benefit: 7.0,
                callee_size: 10,
            },
            InlineCandidate {
                call_site_bci: 2,
                callee_method_id: 3,
                callee_class: "".into(),
                callee_name: "".into(),
                estimated_benefit: 1.0,
                callee_size: 10,
            },
        ];
        policy.rank_candidates(&mut cands);
        assert_eq!(cands[0].estimated_benefit, 7.0);
        assert_eq!(cands[1].estimated_benefit, 3.0);
        assert_eq!(cands[2].estimated_benefit, 1.0);
    }

    // ---- PgoRepository ------------------------------------------------------

    #[test]
    fn repo_get_or_create_inserts() {
        let mut repo = PgoRepository::new();
        repo.get_or_create(1, "A", "m", "()V");
        assert_eq!(repo.total_methods, 1);
        assert!(repo.get(1).is_some());
    }

    #[test]
    fn repo_get_or_create_idempotent() {
        let mut repo = PgoRepository::new();
        repo.get_or_create(1, "A", "m", "()V");
        repo.get_or_create(1, "A", "m", "()V");
        assert_eq!(repo.total_methods, 1);
    }

    #[test]
    fn repo_get_missing_returns_none() {
        let repo = PgoRepository::new();
        assert!(repo.get(999).is_none());
    }

    #[test]
    fn repo_get_hot_methods() {
        let mut repo = PgoRepository::new();
        {
            let p = repo.get_or_create(1, "A", "hot", "()V");
            p.invocation_count = 50_000;
        }
        {
            let p = repo.get_or_create(2, "A", "cold", "()V");
            p.invocation_count = 5;
        }
        let hot = repo.get_hot_methods(10_000);
        assert_eq!(hot.len(), 1);
        assert_eq!(hot[0].method_id, 1);
    }

    #[test]
    fn repo_serialize_summary_counts() {
        let mut repo = PgoRepository::new();
        {
            let p = repo.get_or_create(1, "A", "m", "()V");
            p.invocation_count = 100_000;
            p.record_branch(0, true);
            p.record_branch(0, true);
            p.record_branch(0, false);
            // Make biased branch at BCI 5.
            for _ in 0..95 {
                p.record_branch(5, true);
            }
            p.record_branch(5, false);
            for i in 0..3u32 {
                p.record_receiver(10, i, i as u64);
            }
            for _ in 0..10000 {
                p.record_backedge(20);
            }
        }
        let summary = repo.serialize_summary();
        assert_eq!(summary.total_methods, 1);
        assert_eq!(summary.hot_methods, 1);
        assert_eq!(summary.biased_branches, 1);
        assert!(summary.total_loops > 0);
        assert!(summary.hot_loops > 0);
    }

    #[test]
    fn repo_merge_accumulates_counts() {
        let mut r1 = PgoRepository::new();
        {
            let p = r1.get_or_create(1, "A", "m", "()V");
            p.invocation_count = 100;
            p.record_branch(0, true);
        }
        let mut r2 = PgoRepository::new();
        {
            let p = r2.get_or_create(1, "A", "m", "()V");
            p.invocation_count = 200;
            p.record_branch(0, false);
        }
        r1.merge(r2);
        let p = r1.get(1).unwrap();
        assert_eq!(p.invocation_count, 300);
        assert_eq!(p.branches[&0].taken_count, 1);
        assert_eq!(p.branches[&0].not_taken_count, 1);
    }

    #[test]
    fn repo_merge_new_method() {
        let mut r1 = PgoRepository::new();
        r1.get_or_create(1, "A", "m", "()V");
        let mut r2 = PgoRepository::new();
        r2.get_or_create(2, "B", "n", "()V");
        r1.merge(r2);
        assert_eq!(r1.total_methods, 2);
    }

    // ---- DevirtualizationAnalyzer -------------------------------------------

    #[test]
    fn devirt_monomorphic_inline() {
        let mut mp = MethodProfile::new(1, "A", "m", "()V");
        for _ in 0..200 {
            mp.record_call(10, 99);
        }
        mp.record_receiver(10, 5, 99);
        let analyzer = DevirtualizationAnalyzer;
        let policy = InliningPolicy::default();
        let decisions = analyzer.analyze(&mp, &policy);
        let d = decisions.iter().find(|d| d.call_site_bci == 10).unwrap();
        assert_eq!(d.strategy, DevirtStrategy::Inline(99));
        assert!(d.confidence > 0.9);
    }

    #[test]
    fn devirt_bimorphic_if_then_else() {
        let mut mp = MethodProfile::new(1, "A", "m", "()V");
        for _ in 0..5 {
            mp.record_receiver(20, 1, 101);
        }
        for _ in 0..5 {
            mp.record_receiver(20, 2, 202);
        }
        let analyzer = DevirtualizationAnalyzer;
        let policy = InliningPolicy::default();
        let decisions = analyzer.analyze(&mp, &policy);
        let d = decisions.iter().find(|d| d.call_site_bci == 20).unwrap();
        match &d.strategy {
            DevirtStrategy::IfThenElse(a, b) => {
                assert!(*a == 101 || *a == 202);
                assert!(*b == 101 || *b == 202);
                assert_ne!(a, b);
            }
            other => panic!("expected IfThenElse, got {other:?}"),
        }
    }

    #[test]
    fn devirt_megamorphic() {
        let mut mp = MethodProfile::new(1, "A", "m", "()V");
        for i in 0..5u32 {
            mp.record_receiver(30, i, i as u64 + 100);
        }
        let analyzer = DevirtualizationAnalyzer;
        let policy = InliningPolicy::default();
        let decisions = analyzer.analyze(&mp, &policy);
        let d = decisions.iter().find(|d| d.call_site_bci == 30).unwrap();
        assert_eq!(d.strategy, DevirtStrategy::Megamorphic);
    }

    #[test]
    fn devirt_direct_call_rare() {
        // Monomorphic but below min_call_count => DirectCall, not Inline.
        let mut mp = MethodProfile::new(1, "A", "m", "()V");
        for _ in 0..10 {
            mp.record_call(40, 77);
        }
        mp.record_receiver(40, 9, 77);
        let analyzer = DevirtualizationAnalyzer;
        let policy = InliningPolicy::default(); // min_call_count = 100
        let decisions = analyzer.analyze(&mp, &policy);
        let d = decisions.iter().find(|d| d.call_site_bci == 40).unwrap();
        assert_eq!(d.strategy, DevirtStrategy::DirectCall(77));
    }

    // ---- Serialisation round-trip -------------------------------------------

    #[test]
    fn serialization_roundtrip_empty() {
        let repo = PgoRepository::new();
        let bytes = ProfileSerializer::serialize(&repo);
        let restored = ProfileSerializer::deserialize(&bytes).unwrap();
        assert_eq!(restored.total_methods, 0);
    }

    #[test]
    fn serialization_roundtrip_with_data() {
        let mut repo = PgoRepository::new();
        {
            let p = repo.get_or_create(42, "java/lang/Object", "hashCode", "()I");
            p.invocation_count = 9999;
            p.record_branch(0, true);
            p.record_branch(0, false);
            p.record_receiver(10, 7, 77);
            p.record_call(20, 55);
            for _ in 0..100 {
                p.record_backedge(30);
            }
            p.deopt.record_deopt("type_mismatch", 5);
        }
        let bytes = ProfileSerializer::serialize(&repo);
        let restored = ProfileSerializer::deserialize(&bytes).unwrap();
        let rp = restored.get(42).unwrap();
        assert_eq!(rp.invocation_count, 9999);
        assert_eq!(rp.backedge_count, 100);
        assert_eq!(rp.class_name, "java/lang/Object");
        assert_eq!(rp.method_name, "hashCode");
        assert_eq!(rp.descriptor, "()I");
        assert!(rp.branches.contains_key(&0));
        assert!(rp.type_profiles.contains_key(&10));
        assert!(rp.call_sites.contains_key(&20));
        assert!(rp.loops.contains_key(&30));
        assert_eq!(rp.deopt.deopt_count, 1);
    }

    #[test]
    fn serialization_bad_magic_error() {
        let mut bytes = ProfileSerializer::serialize(&PgoRepository::new());
        bytes[0] ^= 0xFF; // corrupt magic
        assert!(ProfileSerializer::deserialize(&bytes).is_err());
    }

    #[test]
    fn serialization_bad_version_error() {
        let mut bytes = ProfileSerializer::serialize(&PgoRepository::new());
        // Version is at bytes 4..6 (after 4-byte magic).
        bytes[4] = 0xFF;
        bytes[5] = 0xFF;
        assert!(ProfileSerializer::deserialize(&bytes).is_err());
    }

    #[test]
    fn serialization_truncated_error() {
        let repo = {
            let mut r = PgoRepository::new();
            r.get_or_create(1, "A", "m", "()V");
            r
        };
        let bytes = ProfileSerializer::serialize(&repo);
        // Chop the last 10 bytes.
        let truncated = &bytes[..bytes.len() - 10];
        assert!(ProfileSerializer::deserialize(truncated).is_err());
    }

    #[test]
    fn serialization_multiple_methods() {
        let mut repo = PgoRepository::new();
        repo.get_or_create(1, "A", "a", "()V").invocation_count = 10;
        repo.get_or_create(2, "B", "b", "()V").invocation_count = 20;
        repo.get_or_create(3, "C", "c", "()V").invocation_count = 30;
        let bytes = ProfileSerializer::serialize(&repo);
        let restored = ProfileSerializer::deserialize(&bytes).unwrap();
        assert_eq!(restored.total_methods, 3);
        assert_eq!(restored.get(1).unwrap().invocation_count, 10);
        assert_eq!(restored.get(2).unwrap().invocation_count, 20);
        assert_eq!(restored.get(3).unwrap().invocation_count, 30);
    }
}
