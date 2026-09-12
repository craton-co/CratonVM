// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! # Semantic differential fuzzer vs HotSpot
//!
//! Industrializes the manual "run it on HotSpot and eyeball the diff" loop
//! that has produced nearly every bug in `*` and `MEMORY.md`.
//! Generate / mutate Java programs, run them on **both** CratonVM and a real
//! JDK, and **diff observable behavior** — stdout, stderr, thrown exception
//! type + message, and process exit code — automatically.
//!
//! Design: `docs/feature-designs/differential-fuzzer.md`.
//!
//! ## Status: complete (Steps 0–7)
//!
//! The runner spawns the `cratonvm` binary and a real `java` as subprocesses,
//! compiles a `.java` program once with `javac`, and runs the same `.class` on
//! both VMs; the oracle diffs the four channels under strict-by-default
//! normalization; the [`harness`] fans CratonVM across **every configured
//! mode** ([`runner::Mode`]) against one HotSpot run, classifies each
//! divergence, runs the determinism pre-flight + re-confirmation, and
//! [`gate`](harness::gate)s a run against the committed [`ledger::Ledger`].
//! [`generate`] emits a seeded type-directed corpus; [`mutate`] is the
//! format-aware bytecode mutator (also a libFuzzer target +
//! OSS-Fuzz-onboarded); [`minimize`] ddmin-shrinks a confirmed divergence to a
//! minimal repro under `difftest/regression/`. The macro tier reuses
//! `cratonvm-difftest run` over any directory of real programs (`--check-determinism`
//! to reject flaky ones); see `README.md`.
//!
//! The cooperating pieces, each its own module:
//!
//! | Module          | Role (design §)             | State          |
//! |-----------------|-----------------------------|----------------|
//! | [`ledger`]      | divergence record + JSON    | wired          |
//! | [`runner`]      | two-VM A/B executor (§3.2)  | wired (matrix) |
//! | [`oracle`]      | diff + classify (§3.3)      | wired          |
//! | [`harness`]     | compile + run + diff + gate | wired (matrix) |
//! | [`generate`]    | corpus generator (§3.1)     | wired (3 families) |
//! | [`mutate`]      | bytecode mutator (§3.1 t3)  | wired          |
//! | [`minimize`]    | ddmin shrink (§3.4)         | wired          |
//! | [`census`]      | JDK-only dump reader (§9)   | wired          |
//!
//! ## JDK-only mode (`docs/feature-designs/jdk-only-mode.md`)
//!
//! Four additional [`runner::Mode`]s select a *compatibility policy* on the
//! launcher command line (`--jdk-only` / `--real-jdk`) and collect the three
//! census dumps [`census`] reads back. Because the policy is part of what was
//! measured, a ledger row is keyed by **`(class, jdk_profile)`**: the same
//! program diverging under the strict policy and under the compatible one is
//! two findings, and a `known` compatible row can never excuse a strict
//! divergence. The five historical modes are untouched — same labels, same
//! argv, same env, same rows — so the committed `difftest/seeds` gate baseline
//! keeps measuring exactly what it always did.

//! ## C2 review: dimensions, paths, and a generated matrix
//!
//! Three additions answer the C2 review's differential-testing items, and each
//! is built so that a reported divergence is *actionable evidence* rather than a
//! raw diff:
//!
//! * [`normalize`] turns every transform the oracle may apply into a **named
//!   rule** with a documented target, justification and risk. An
//!   un-normalized nondeterminism is a false-positive generator; a silent
//!   over-normalization hides a real bug. Both are review-able only if the
//!   rules have names.
//! * [`checksum`] adds the report's fifth channel — a quantity the *program*
//!   computes and declares. It is read from un-normalized stdout, so no rule
//!   can launder it, and a divergence names the quantity rather than a byte
//!   offset.
//! * [`crossmode`] compares CratonVM's execution paths **against each other**.
//!   The VM has four semantic implementations (interpreter fast path with
//!   superinstructions, interpreter decoded fallback, single-pass direct
//!   emitter, optimizing IR pipeline) plus OSR and deopt as transitions; a fix
//!   landing in one is a wrong-code risk in the others. Two of the VM's own
//!   paths disagreeing needs no reference JDK to be a defect.
//! * [`matrix`] derives, from the corpus class files themselves, which opcodes
//!   and operand forms each path actually reaches — so the gaps are visible
//!   instead of assumed. Each `(opcode, axis)` cell is **covered**, **uncovered**
//!   with the gap that names its fix, or **unreachable-by-construction** with
//!   the reason; a boolean cell could not tell "nobody wrote a seed" from
//!   "nothing can write one", and `jsr` is permanently the second.
//! * [`opcorpus`] is what makes that number mean something. A matrix over the
//!   three committed seeds reports on roughly four of 202 opcodes; this emits a
//!   deterministic, checksum-declaring Java program per opcode — 197 of them,
//!   with the five `javac` cannot produce named and explained — each wrapping
//!   its focus in a loop and a handler so the `osr` and `exception` columns have
//!   code sites at all.
//!
//! `docs/testing/differential.md` is the operator-facing write-up: what is
//! compared, every normalization rule, the mode axis and the real VM flags it
//! sets, how to read the matrix, and how to tell a harness false positive from
//! a real VM divergence.

//! ## JIT correctness lanes
//!
//! Two additions target the JIT specifically, and neither needs a reference
//! JDK — the interpreter (`nojit`) is the oracle:
//!
//! * [`pathgate`] gates a corpus on every execution-path mode agreeing with
//!   `nojit`. CI runs it over `difftest/seeds` for the IR, OSR and deopt modes
//!   the HotSpot gate does not cover, so those modes need no ledger rows.
//! * [`jitfuzz`] generates small verifiable class files straight from bytecode
//!   (through the typed assembler in [`classgen`]), biased toward the
//!   semantics JITs get wrong, and reports any mode whose output differs from
//!   `nojit`, minimizing the failing class for triage.
//!
//! Operator guide: `docs/testing/jit-differential.md`.

pub mod census;
pub mod checksum;
pub mod classgen;
pub mod crossmode;
pub mod generate;
pub mod harness;
pub mod jitfuzz;
pub mod ledger;
pub mod matrix;
pub mod minimize;
pub mod mutate;
pub mod normalize;
pub mod opcorpus;
pub mod oracle;
pub mod pathgate;
pub mod runner;
