// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A trap in a compiled body reached through ORDINARY BYTECODE DISPATCH must
//! resume that body, not re-enter it from bci 0 and run its side effects twice.
//!
//! # The defect this pins
//!
//! A deopt sentinel does not mean "nothing happened". It means the compiled
//! body ran up to the trapping bci and stopped. `resume_deopted_body`'s own doc
//! says so in as many words — *"This is the only correct answer for a body that
//! has already committed a side effect"* — and cites the hibernate-reactive
//! `reactiveRemove`-fires-twice defect that came of a sink dropping the frame.
//!
//! Three sinks in `jit_bridge.rs` nevertheless answered it by re-running:
//! `execute_jit_call` (`jit-callsite-a`, the `invokestatic` path),
//! `execute_jit_call_decoded` (`jit-callsite-b`, the virtual/interface path)
//! and `resume_deopted_body` (both one-shot lambda doors). Each tried a precise
//! resume behind two preconditions that are false on every optimizing-tier
//! artifact in a production build —
//!
//! * `ir_deopt_resume_enabled()` (`CRATONVM_IR_DEOPT_RESUME`) is default OFF,
//!   and its own comment justifies that with *"no production IR method emits a
//!   deopt guard yet"*, which stopped being true;
//! * `compiled.can_deopt_resume` is set by `ir_lower` only under
//!   `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`
//!
//! — and then fell through to `Ok(None)` / `CacheMiss`, which means "re-execute
//! this method from entry". No side-effect check guards that fallback, unlike
//! the tier-up sink's, so the duplication is silent.
//!
//! # The measurement
//!
//! `DeoptRerunCount.hot` stores into an array (an `iastore`, which is
//! `opcode_commits_side_effect`) and then divides (an `idiv`, which the IR tier
//! lowers to a *deopt guard*, not a throw). `trip()` reads the counter either
//! side of one trapping call and returns the delta:
//!
//! | delta | meaning |
//! |---|---|
//! | **1** | the frame was resumed at the trapping bci — the store ran once |
//! | **2** | the method was re-entered from bci 0 — the store ran TWICE |
//!
//! The exception is not the subject. `trip()` catches the `ArithmeticException`
//! the re-executed `idiv` raises; what this file asserts is the COUNT.
//!
//! # Why EVERYTHING here goes through Java, warm-up included
//!
//! `vm.invoke` enters `execute`, and that matters twice over.
//!
//! For the trip: `execute`'s own tier-up sink was fixed on 2026-09-07 and
//! resumes correctly, so calling `hot` from Rust would measure the wrong sink.
//! `trip()` is Java, so its `invokestatic hot` is a real dispatch through
//! `dispatch_static` into `execute_jit_call` -- the sink under test.
//!
//! For the warm-up the reason is different, and was found the hard way. With
//! `CRATONVM_JIT_C2_FIRST_CALL` off (the default), `execute`'s first-call path
//! compiles with the SINGLE-PASS backend directly and caches a non-IR body,
//! "which preempts the optimizing IR pipeline on every later path (dispatcher
//! warmup, OSR, background worker all probe `jit_cache` first)" -- its own
//! words. Warming `hot` through `vm.invoke` therefore pinned it at C1 forever,
//! where `can_deopt_resume` IS set and the defect is invisible. The
//! anti-vacuity check below caught exactly that on the first run, which is the
//! whole reason it checks the BACKEND and not merely that an artifact exists.
//! Warming through `warm()` keeps every call to `hot` on the
//! invocation-counted dispatch path, which is the one that reaches C2.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/DeoptRerunCount";
const HOT: &str = "hot";
const HOT_DESC: &str = "(II)I";

/// `warm(ITERS_PER_WARM)` per outer call; the PRODUCT is what has to clear
/// `TIER_OVERRIDES`' C2 threshold, since each inner iteration is one dispatch
/// of `hot`.
const WARM_CALLS: i32 = 200;
const ITERS_PER_WARM: i32 = 64;
