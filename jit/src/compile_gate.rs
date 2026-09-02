// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The one door every backend entry point must pass through.
//!
//! The `osr-01` lane brief (retired to
//! `osr-01-entry-metadata-contract-RETIRED-20260804.md`) asks for "a single
//! entry point for *produce an OSR-capable artifact*, so the direct path in
//! `invoke.rs` and the `try_compile` path cannot drift again". This is that
//! entry point, in the shape `docs/feature-designs/jit-osr-entry-metadata.md`
//! step 3 settled on: **a shared gate function every door must call**, with the
//! drift made observable instead of invisible.
//!
//! # There are THREE doors, not two
//!
//! The brief says two. [`jit_force_interpret`](crate::jit_force_interpret)'s
//! own doc already names three, because it had to be taught the same lesson
//! from the other side:
//!
//! | Door | Where | Reaches the backend via |
//! |---|---|---|
//! | [`CompileDoor::MethodEntry`] | `try_compile_with_invokespecial_resolver` | `try_compile_inner` → `x64::compile_with_param_slots` |
//! | [`CompileDoor::EagerFirstCall`] | `execute`'s first-call compile (`vm/src/runtime/interpreter.rs`) | `x64::compile_with_param_slots` directly |
//! | [`CompileDoor::Osr`] | `compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`) | `x64::compile_with_param_slots` directly |
//!
//! Only the first went through the admission checks. The other two grew
//! *hand-copied* subsets of them, each added reactively after its own bug:
//! the permanent bail-list (after 35 923 wasted pipelines on `Nat.inc`), the
//! bisect levers (after every bisect step on the annotation-scan SIGSEGV — the
//! retired `annotation-scan-arrayread-sigsegv` write-up — read "no effect"
//! while 11 methods were still compiling), and the OSR-entry-rejected
//! memo (after 256 re-compiles over ten H2 operations). Three separate
//! discoveries of one fact.
//!
//! # What the gate asks, and why each one belongs to every door
//!
//! 1. **The kill switch** (`CRATONVM_DISABLE_JIT`). Previously checked by the
//!    OSR door and by the VM-side callers, but not by `try_compile` itself.
//! 2. **The permanent bail-list.** A method the backend already refused cannot
//!    start succeeding; re-running the pipeline is pure waste. This is the
//!    check `compile_osr_artifact` hand-copied.
//! 3. **The bisect levers** (`CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY`).
//!    A lever that cannot actually stop a compile makes a bisect *lie*, which
//!    is worse than a missing feature — see [`crate::jit_force_interpret`].
//! 4. **The code-cache cap.** Never checked by either direct door: an OSR or
//!    first-call compile could commit code past a cap the ordinary door was
//!    respecting.
//!
//! And two things the gate *does*, rather than asks:
//!
//! 5. **Opens the compile-epoch witness** ([`crate::open_compile_epoch_witness`])
//!    so the artifact is stamped from before the first constant-pool read, not
//!    from buffer finalize. The OSR door opened one, but ~1 000 lines late —
//!    after all of its class loading and CP resolution — so a redefinition
//!    landing in that window produced a body stamped current and published.
//! 6. **Clears the one-shot bail-site record** so a bail can only ever report
//!    its own cause.
//!
//! # How a future drift is stopped, and then caught
//!
//! Two layers, because they fail differently.
//!
//! **The type system stops the accident.**
//! `x64::compile_with_param_slots` takes `&CompileAdmission`, and the only way
//! to get one is [`admit`]. A fourth door written without the gate does not
//! compile. The brief this closes asked for the two paths to be unable to
//! "drift again", and a counter asserted zero by a test is *will be caught*,
//! not *cannot*; this is the *cannot* half.
//!
//! **The counter catches the deliberate bypass.** The `jit` crate's own tests
//! drive the backend with hand-built bytecode and no method identity, so they
//! need a way in: [`CompileAdmission::for_backend_test`], which the legacy
//! `x64::compile` wrapper also uses on their behalf. That is a genuine hole, so
//! it is built not to hide anything — it does not open the thread scope, so a
//! backend entry made under it is still counted by [`ungated_backend_entries`],
//! which the VM asserts is zero over a real run. Its name is what makes a
//! production use greppable.
//!
//! Only `compile_with_param_slots` takes the token, not the `compile` wrapper
//! above it: that wrapper's "arg index == JVM slot" assumption is wrong for any
//! method with a `long`/`double` parameter, so no production path can use it,
//! and gating it would have meant editing ~140 unit-test call sites to protect
//! a function production cannot reach.
//!
//! Both layers are behaviour-named rather than source-scanning: a check that
//! grepped for `compile_with_param_slots(` would have died the day `x64.rs`
//! was split, as five checks in this repository did.
//!
//! # The direct-call plan is a SECOND thing every door builds, and the gate
//! # did not cover it — H20-1
//!
//! `compile_with_param_slots` takes `direct_calls: Vec<(usize, JitDirectCall)>`
//! as **data**, and each door builds its own. `H12-1` MEASURED the
//! consequence on 2026-08-20: under `--jdk-only` the [`CompileDoor::MethodEntry`]
//! ladder examined seven `invokestatic` sites and refused all seven, while
//! [`CompileDoor::Osr`] bound `Thread.currentThread` — a `bridge` row — and
//! compiled code called it 298 000 times. `--real-jdk` reported the identical
//! per-door profile, i.e. that door never asked which mode it was in.
//!
//! **The `CompileAdmission` trick does not transplant onto this.** It works for
//! the backend entry because the refusal is safe at a choke point downstream of
//! every door: "do not compile" is a fallback every caller already has. A
//! direct-bind refusal has no downstream point at all. `x64/driver.rs`'s
//! `reserve_stack_floor` walk defines a raw self-call as *an `invokestatic` pc
//! with neither an invoke-info entry nor a direct-call plan*, and every ladder
//! pushes its row and then `continue`s past the `invoke_info` construction for
//! that pc — so a row dropped after the door leaves the pc with no metadata and
//! the backend compiles `Thread.currentThread()` as a call to the enclosing
//! method. Filtering downstream would trade an open door for a wild jump.
//!
//! So this module supplies the pieces and the door does the refusing, at the
//! bind site, in the shape `try_compile_inner` already uses
//! (`if entry != 0 { push; continue; }` — a refusal *falls through*):
//!
//! * [`DirectCallPolicy`] — the witness, with **no** `Default`, and three
//!   states rather than two so "never asked" stays representable;
//! * [`CompileAdmission::declare_direct_call_policy`] /
//!   [`CompileAdmission::admits_direct_bind`] — the rule, stated once;
//! * [`CompileDoor::builds_direct_calls`] — an exhaustive `match`, so a fourth
//!   door cannot be added without answering it;
//! * [`undeclared_direct_bind_rows`] — the counter, mirroring
//!   [`ungated_backend_entries`]: it does not stop the accident, it makes the
//!   bypass a number instead of a silence.
//!
//! The VM half is `vm/src/jit/helpers.rs::admit_direct_native_entry`, which is
//! where the registry's `NativeKind` can actually be read.
//!
//! # The String-intrinsic pin was installed at ONE door and its population
//! # takes another — C1, 2026-09-01
//!
//! `jit/src/lib.rs::string_intrinsic_pin_declines` — *a method with a
//! `java/lang/String` access-intrinsic call site stays on the single-pass
//! backend, because the optimizing tier silently loses the intrinsic* — was a
//! term of `try_compile_inner`'s eligibility conjunction and nothing else.
//! `try_compile_inner` is [`CompileDoor::MethodEntry`], one of the three doors
//! the table above enumerates. MEASURED on `probes/CharAtWarmShape.java`, one
//! binary, three arms:
//!
//! ```text
//! arm A  default (pin on)                                314-336 ns/char
//! arm B  CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1           66-108 ns/char  ~3x
//! arm C  pin off + CRATONVM_JIT_IR_STRING_INTRINSICS=0       421 ns/char  (control)
//!
//! [cratonvm] JIT String-intrinsic pin: fired=0 blind-no-layout=0
//!            blind-no-resolver=0 fail-closed=0
//! [cratonvm-jitc] OSR-compile CharAtWarmShape.scanBig(...)I entry_pc=12
//! ```
//!
//! All four of the pin's counters read zero on the very workload the pin
//! exists to govern, while the switch that lifts the pin moved that workload
//! 3x. A counter installed at one door reports zero for traffic through
//! another, and a zero reads as *correctly inert*. That is how the audit
//! behind `string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901`
//! refuted five hypotheses without converging: every one of them was about
//! which *method* takes the fast shape, and the discriminator was a *door*.
//!
//! So the asking moved here, onto the token every door already holds:
//! [`CompileAdmission::string_intrinsic_pin_declines`]. The DECISION did not
//! move and did not change — it is still stated once, in `lib.rs`, and arm C
//! is the control that says it is right (lifting the pin *without* the IR
//! String emitter is worse than the default, 421 against 336). What is new is
//! that asking is a per-door fact: [`string_pin_asked`],
//! [`string_pin_declined`], and the one that would have named this in a single
//! run, [`string_pin_not_asked`], which [`CompileAdmission`]'s `Drop` charges
//! to any door that held an admission and never put the question. Per door,
//! `asked + not-asked == admissions`, so *this door never asks* is a NUMBER
//! rather than an inference from a zero.
//!
//! [`CompileDoor::asks_string_intrinsic_pin`] is the same fact with the
//! compiler as its reader, for the reason [`CompileDoor::builds_direct_calls`]
//! gives: a fourth door has to answer it before the crate builds.
//!
//! One thing a reader of the counters must know about today's answers. All
//! three doors ask as of 2026-09-01, but at the OSR and eager-first-call doors
//! the pin's decision is, today, *vacuous*: both reach
//! `x64::compile_with_param_slots` (the single-pass backend) unconditionally
//! and have no route to the optimizing tier at all, so "keep this method off
//! the optimizing tier" is already true there. That is incidental correctness
//! of exactly the kind [`CompileDoor::builds_direct_calls`] refuses to leave
//! in a doc sentence — it is a property of today's topology, not of the door —
//! and it is why the counter is `not-asked` rather than a claim that the
//! answer would have been the same.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// Which entry point is asking to compile.
///
/// Recorded per door so "the OSR path calls the gate" is a *measurement*
/// rather than a claim about the source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileDoor {
    /// `try_compile_with_invokespecial_resolver` — the ordinary tiering door.
    MethodEntry,
    /// The interpreter's eager first-call single-pass compile.
    EagerFirstCall,
    /// `compile_osr_artifact` — the on-stack-replacement door.
    Osr,
}

impl CompileDoor {
    /// Every door, for the tests and the diagnostic dump.
    pub const ALL: [CompileDoor; 3] = [
        CompileDoor::MethodEntry,
        CompileDoor::EagerFirstCall,
        CompileDoor::Osr,
    ];

    const fn index(self) -> usize {
        match self {
            CompileDoor::MethodEntry => 0,
            CompileDoor::EagerFirstCall => 1,
            CompileDoor::Osr => 2,
        }
    }

    /// Stable name for diagnostics.
    pub const fn label(self) -> &'static str {
        match self {
            CompileDoor::MethodEntry => "method-entry",
            CompileDoor::EagerFirstCall => "eager-first-call",
            CompileDoor::Osr => "osr",
        }
    }

    /// Whether this door builds its own `direct_calls` plan, and therefore owes
    /// the JDK-only direct-bind question before it binds a helper that shadows
    /// a registered native.
    ///
    /// # Why this is a `match` and not a doc sentence — H20-1
    ///
    /// The module doc above has named three doors since it was written, and
    /// `H12-1` (2026-08-20) still MEASURED two of them binding a `bridge` row
    /// under `--jdk-only` while the third refused all seven sites it examined.
    /// The table was right and nothing read it. This `match` is the same fact
    /// with the compiler as its reader: a fourth `CompileDoor` variant makes it
    /// non-exhaustive, so whoever adds the door has to answer the question
    /// before the crate builds. That is the whole difference between
    /// [`a-premise-in-a-comment-is-not-a-compile-time-link`] and a link.
    ///
    /// `true` for all three today. Kept as an explicit per-variant answer
    /// rather than `true` because the answer is a property of the door's
    /// ladder, not of the enum: a future door that reaches the backend with
    /// `Vec::new()` owes nothing, and should be able to say so here.
    ///
    /// [`a-premise-in-a-comment-is-not-a-compile-time-link`]: crate::compile_gate
    pub const fn builds_direct_calls(self) -> bool {
        match self {
            // `try_compile_inner`'s single-pass and IR ladders. The one door
            // that asks: every bind goes through `crate::direct_native_helper`
            // or `direct_native_helper_for_impl`.
            CompileDoor::MethodEntry => true,
            // `direct_calls_early` — `Math.sqrt`, `Integer.valueOf(I)`,
            // `Integer.intValue()`. Both registered rows are `intrinsic`, so
            // this door is *incidentally* correct and structurally unguarded.
            CompileDoor::EagerFirstCall => true,
            // `direct_calls2` — eleven push sites, five of them `bridge` rows.
            // The door H12-1 measured binding in strict mode.
            CompileDoor::Osr => true,
        }
    }

    /// Whether this door asks the String-intrinsic pin
    /// ([`CompileAdmission::string_intrinsic_pin_declines`]) before it reaches
    /// the backend.
    ///
    /// # Why this is a `match` and not a doc sentence — C1, 2026-09-01
    ///
    /// The same reason [`builds_direct_calls`] is one, and the same failure a
    /// second time. The pin lived in `try_compile_inner`'s conjunction, so
    /// exactly one door asked it; the population that motivated it is compiled
    /// through another; and the instrument that should have said so — four
    /// process-global counters — read `fired=0`, which is indistinguishable
    /// from *there was nothing to pin*. See the module doc for the three-arm
    /// measurement.
    ///
    /// Today `MethodEntry` is the only `true`. The other two are `false` and
    /// are COUNTED as such by [`string_pin_not_asked`] rather than assumed
    /// harmless: a `false` here is a statement about what the door does, not a
    /// claim that its answer would not have mattered.
    ///
    /// [`builds_direct_calls`]: CompileDoor::builds_direct_calls
    pub const fn asks_string_intrinsic_pin(self) -> bool {
        match self {
            // `try_compile_inner`'s eligibility conjunction, through the
            // admission token.
            CompileDoor::MethodEntry => true,
            // `vm/src/runtime/interpreter.rs` — reaches
            // `x64::compile_with_param_slots` directly, and ASKS as of
            // 2026-09-01. It passes the same `string_layout: None` the backend
            // call below it passes, so the answer is always `BlindNoLayout` —
            // fail OPEN, and counted. Deliberate: without a layout the
            // single-pass backend emits no intrinsic either, so pinning would
            // cost a C2 body and buy nothing, and `BlindNoLayout` is the arm
            // that says so. `BlindNoResolver` would not.
            CompileDoor::EagerFirstCall => true,
            // `vm/src/runtime/interpreter/jit_bridge.rs` — reaches
            // `x64::compile_with_param_slots` directly. It resolves a
            // `java/lang/String` field layout (`osr_string_layout`) and walks
            // the constant pool per invoke site, so it supplies BOTH of the
            // pin's inputs and can reach `Pinned`, not merely a blind arm. It
            // asks as of 2026-09-01; before that it never had been, which is
            // exactly why `fired=0` on the probe the pin exists for.
            CompileDoor::Osr => true,
        }
    }

    /// Where this door's direct-call ladder lives, for a refusal message that
    /// names the file the reader has to open.
    ///
    /// Three files in two crates, which is `H12-2` §1a's third reason a
    /// call-site grep could not produce the bind matrix.
    pub const fn direct_call_ladder(self) -> &'static str {
        match self {
            CompileDoor::MethodEntry => "jit/src/lib.rs::try_compile_inner",
            CompileDoor::EagerFirstCall => "vm/src/runtime/interpreter.rs::direct_calls_early",
            CompileDoor::Osr => "vm/src/runtime/interpreter/jit_bridge.rs::direct_calls2",
        }
    }
}

/// The execution policy in force for one compilation, as the direct-call
/// ladders must see it.
///
/// # Why a two-variant enum and not `bool`
///
/// There are three states a door can be in, not two: `Compatible`, `JdkOnly`,
/// and **never asked**. A `bool` collapses the third into the first, which is
/// exactly the shape of the defect — the OSR door has been passing an implicit
/// "compatible" to a question it never knew existed, and `--real-jdk` and
/// `--jdk-only` MEASURED an identical bind profile because of it. Keeping
/// "undeclared" representable is what lets [`undeclared_direct_bind_rows`]
/// count it.
///
/// Deliberately not `Default`: there is no defensible default, and deriving one
/// would re-create the collapse this type exists to prevent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectCallPolicy {
    /// `--real-jdk`. Every thin helper may bind.
    Compatible,
    /// `--jdk-only`. Only a reviewed `NativeKind::Intrinsic` row may bind;
    /// a `Bridge` row must defer to the real JDK bytecode.
    JdkOnly,
}

impl DirectCallPolicy {
    /// Build from the VM's `is_jdk_only()`.
    ///
    /// Named rather than `From<bool>` so the call site reads as an answer to a
    /// question, and so a grep for the answer finds every door.
    pub const fn from_jdk_only(jdk_only: bool) -> Self {
        if jdk_only {
            DirectCallPolicy::JdkOnly
        } else {
            DirectCallPolicy::Compatible
        }
    }

    /// Whether this policy admits a direct bind onto a shadow of a registered
    /// native whose kind is (or is not) a reviewed `Intrinsic`.
    ///
    /// The rule is `crate::direct_native_helper`'s, stated once: `Compatible`
    /// admits everything; `JdkOnly` admits `Intrinsic` and nothing else —
    /// `Bridge`, `SyntheticStub` and unregistered all refuse, which is the
    /// fail-closed direction.
    pub const fn admits_shadow_bind(self, is_reviewed_intrinsic: bool) -> bool {
        match self {
            DirectCallPolicy::Compatible => true,
            DirectCallPolicy::JdkOnly => is_reviewed_intrinsic,
        }
    }
}

/// Why the gate refused.
///
/// Carries no payload: every variant is a whole-method verdict whose inputs are
/// process state, and the method identity is the caller's. What a caller does
/// with the distinction differs — `PermanentlyBailListed` is forever,
/// `CodeCacheAtCapacity` is transient — so they are kept apart rather than
/// collapsed into a bool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileRefusal {
    /// `CRATONVM_DISABLE_JIT` — interpreter-only execution.
    JitDisabled,
    /// The backend already refused this method; the bytecode has not changed.
    PermanentlyBailListed,
    /// `CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY` force-interpret it.
    ForceInterpreted,
    /// Retained JIT code is at the configured cap. Transient: reclaiming a body
    /// restores headroom.
    CodeCacheAtCapacity,
}

impl CompileRefusal {
    /// Whether re-asking later could produce a different answer.
    ///
    /// The bail-list is the one verdict that is a pure function of the
    /// bytecode; the other three are configuration or resource state.
    pub const fn is_transient(self) -> bool {
        matches!(self, CompileRefusal::CodeCacheAtCapacity)
    }
}

impl std::fmt::Display for CompileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CompileRefusal::JitDisabled => "CRATONVM_DISABLE_JIT",
            CompileRefusal::PermanentlyBailListed => "permanently bail-listed",
            CompileRefusal::ForceInterpreted => "force-interpreted by a bisect lever",
            CompileRefusal::CodeCacheAtCapacity => "code-cache cap reached",
        };
        f.write_str(s)
    }
}

static ADMISSIONS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static REFUSALS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static UNGATED_BACKEND_ENTRIES: AtomicU64 = AtomicU64::new(0);
/// Direct-call rows that reached the backend under an admission whose door
/// never declared a [`DirectCallPolicy`]. See [`undeclared_direct_bind_rows`].
static UNDECLARED_DIRECT_BIND_ROWS: [AtomicU64; 3] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
/// Compilations at which this door put the String-intrinsic pin's question.
/// See [`string_pin_asked`] and the module doc's "installed at ONE door".
static STRING_PIN_ASKED: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
/// Compilations at which the pin, asked through this door, said *pin it*.
static STRING_PIN_DECLINED: [AtomicU64; 3] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
/// Compilations this door held an admission for and never asked the pin at
/// all. Charged by [`CompileAdmission`]'s `Drop`, so no door has to remember.
static STRING_PIN_NOT_ASKED: [AtomicU64; 3] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

thread_local! {
    /// Depth of open admissions on this thread. A count rather than a flag
    /// because `callee_compiler` recursion nests one compile inside another.
    static OPEN_ADMISSIONS: Cell<u32> = const { Cell::new(0) };
}

/// Proof that a compile passed the admission gate.
///
/// Hold it for the whole compilation: it owns the compile-epoch witness, so
/// dropping it early re-narrows the install-epoch window this exists to widen.
#[must_use = "the admission token must stay alive for the whole compilation — \
              it owns the compile-epoch witness"]
pub struct CompileAdmission {
    door: CompileDoor,
    // Dropped after the counter below, which is what makes "an admission is
    // open" and "the epoch witness is open" the same interval.
    _epoch: crate::CompileEpochWitness,
    /// Whether this token incremented the thread scope. `false` only for
    /// [`CompileAdmission::for_backend_test`], whose whole point is that it
    /// does not — see there.
    opened_scope: bool,
    /// `0` = the door never declared one, `1` = `Compatible`, `2` = `JdkOnly`.
    ///
    /// An `AtomicU8` rather than a `Cell` so the token keeps its auto traits: a
    /// `Cell` would make `CompileAdmission` `!Sync`, and this type is held
    /// across a whole compilation whose shape this lane could not build and
    /// check. Relaxed throughout — the value is written and read by the one
    /// thread that owns the compilation.
    direct_call_policy: std::sync::atomic::AtomicU8,
    /// Whether this compilation ever put the String-intrinsic pin's question —
    /// see [`CompileAdmission::string_intrinsic_pin_declines`].
    ///
    /// An atomic behind `&self` for the same reason `direct_call_policy` above
    /// is one: a `Cell` would make `CompileAdmission` `!Sync`, and the token is
    /// held across a whole compilation. Relaxed throughout; written and read by
    /// the one thread that owns the compilation.
    ///
    /// Read exactly once more, in `Drop`, which is what makes "never asked" a
    /// count nobody has to remember to take.
    string_pin_asked: std::sync::atomic::AtomicBool,
}

const DIRECT_POLICY_UNDECLARED: u8 = 0;
const DIRECT_POLICY_COMPATIBLE: u8 = 1;
const DIRECT_POLICY_JDK_ONLY: u8 = 2;

impl CompileAdmission {
    /// Which door this admission was granted to.
    pub fn door(&self) -> CompileDoor {
        self.door
    }

    /// Declare the execution policy this compilation's direct-call ladder must
    /// obey. Call it once, before building `direct_calls`.
    ///
    /// # What this buys, and what it does not — H20-1
    ///
    /// It does **not** filter anything by itself, and it deliberately cannot:
    /// see [`admits_direct_bind`] and [`note_direct_binds`] for the two halves.
    /// What it buys is that "this door never asked" stops being invisible.
    /// Before it, `--jdk-only` and `--real-jdk` MEASURED an identical per-door
    /// bind profile (`H12-1` §3b) and no counter in the tree could tell a door
    /// that refused from a door that never asked.
    ///
    /// Idempotent and last-write-wins; a door that declares twice with
    /// different answers is a bug this cannot see, which is why the *decision*
    /// is [`admits_direct_bind`]'s and not this method's.
    pub fn declare_direct_call_policy(&self, policy: DirectCallPolicy) {
        let v = match policy {
            DirectCallPolicy::Compatible => DIRECT_POLICY_COMPATIBLE,
            DirectCallPolicy::JdkOnly => DIRECT_POLICY_JDK_ONLY,
        };
        self.direct_call_policy
            .store(v, std::sync::atomic::Ordering::Relaxed);
    }

    /// The policy this compilation's door declared, or `None` if it never did.
    ///
    /// `None` is the state the OSR and eager first-call doors are in as this
    /// lands, and it is the state [`undeclared_direct_bind_rows`] counts.
    pub fn direct_call_policy(&self) -> Option<DirectCallPolicy> {
        match self
            .direct_call_policy
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            DIRECT_POLICY_COMPATIBLE => Some(DirectCallPolicy::Compatible),
            DIRECT_POLICY_JDK_ONLY => Some(DirectCallPolicy::JdkOnly),
            _ => None,
        }
    }

    /// May this compilation bind a direct call onto a helper that shadows a
    /// registered native?
    ///
    /// `is_reviewed_intrinsic` is the VM's answer for the triple whose native
    /// actually runs — the registry's `NativeKind == Intrinsic`, which is
    /// §1.4's reviewed exception. It is a closure because only the strict arm
    /// needs it and only for a row that is otherwise bindable, and because this
    /// crate cannot see `NativeKind` at all.
    ///
    /// # This must be called at the BIND SITE, before the ladder's `continue`
    ///
    /// It is the *only* place a refusal is safe, and that is a property of the
    /// backend, not a style preference. `jit/src/x64/driver.rs`'s
    /// `reserve_stack_floor` walk states the rule: *"a raw self-call site is an
    /// `invokestatic` pc with neither an invoke-info entry nor a direct-call
    /// plan"*. Every ladder pushes its row and then `continue`s **past** the
    /// `invoke_info` construction for that pc, so a row deleted downstream —
    /// in this crate, in the backend, anywhere after the door — leaves the pc
    /// with no metadata at all, and the `0xb8` arm then compiles
    /// `Thread.currentThread()` as a call to the enclosing method.
    ///
    /// `try_compile_inner` already has the correct shape and is the model:
    /// `if entry != 0 { push; continue; }`, so a refusal *falls through* to the
    /// code that builds the fallback. A door must do the same with this.
    ///
    /// # Undeclared is admitted, and counted
    ///
    /// A door that never called [`declare_direct_call_policy`] gets `true`,
    /// because the alternative is worse than the defect: refusing a bind for a
    /// door that has not been taught to fall through would strand the pc in the
    /// no-metadata state described above. The bypass is made loud instead of
    /// safe-by-guess — [`note_direct_binds`] counts it.
    pub fn admits_direct_bind(&self, is_reviewed_intrinsic: impl FnOnce() -> bool) -> bool {
        match self.direct_call_policy() {
            Some(p) => p.admits_shadow_bind(is_reviewed_intrinsic()),
            None => true,
        }
    }

    /// Must this compilation keep the method on the backend that HAS the
    /// `java/lang/String` access intrinsic? `true` = do not promote it to the
    /// optimizing tier.
    ///
    /// # The decision is `lib.rs`'s; only the asking is here — C1, 2026-09-01
    ///
    /// This forwards, unchanged, to `crate::string_intrinsic_pin_declines`,
    /// which is where the rule and its three-way "the input was missing"
    /// treatment are stated once. Nothing about WHAT the pin decides moved:
    /// arm C of the module doc's measurement is the control that says the
    /// decision is right, and a rewrite would have voided it.
    ///
    /// What moved is WHERE it is asked. The pin used to be a bare call inside
    /// `try_compile_inner`, i.e. at [`CompileDoor::MethodEntry`] and nowhere
    /// else, and its four census counters therefore reported `0` on a workload
    /// compiled through the OSR door — see the module doc. The admission token
    /// is the one object all three doors already hold, so a door that wants the
    /// pin's answer can now get it without a fourth hand-copied subset of
    /// `try_compile_inner`, which is the failure this whole module exists for.
    ///
    /// # A door that cannot supply the inputs
    ///
    /// Pass what you have, including `None`. The two missing-input cases are
    /// decided DIFFERENTLY and deliberately, by the same function as before:
    ///
    ///   * no constant-pool invoke resolver, layout present — FAIL CLOSED
    ///     (`true`, keep the method on the backend with the intrinsic), because
    ///     that is a missing input at one door and not evidence about the
    ///     method;
    ///   * no `java/lang/String` field layout — fail OPEN (`false`), because
    ///     without one the single-pass backend emits no intrinsic either, so
    ///     pinning would cost a C2 body and buy nothing.
    ///
    /// Both are counted by the global census (`blind-no-resolver`,
    /// `blind-no-layout`, `fail-closed`) and both count as ASKED here, so a
    /// blind door is visible as a door that asked rather than as a door that
    /// did not.
    ///
    /// # Counting
    ///
    /// The first ask on a token bumps [`string_pin_asked`] for its door, so
    /// `asked + not-asked == admissions` per door even if a door asks twice.
    /// A `true` additionally bumps [`string_pin_declined`]. Not asking at all
    /// is charged in `Drop`.
    pub fn string_intrinsic_pin_declines(
        &self,
        invoke_ops: &[(usize, u16, u8)],
        cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
        layout: Option<crate::StringFieldLayout>,
    ) -> bool {
        let first_ask = !self
            .string_pin_asked
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        if first_ask {
            STRING_PIN_ASKED[self.door.index()].fetch_add(1, Ordering::Relaxed);
        }
        let declines = crate::string_intrinsic_pin_declines(invoke_ops, cp_invoke_resolver, layout);
        if declines {
            STRING_PIN_DECLINED[self.door.index()].fetch_add(1, Ordering::Relaxed);
        }
        declines
    }

    /// Whether this compilation has already asked the pin. Test support and
    /// diagnostics; the counters above are what a run reports.
    pub fn string_intrinsic_pin_was_asked(&self) -> bool {
        self.string_pin_asked
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A token for a **test** that drives the backend directly.
    ///
    /// `x64::compile_with_param_slots` takes `&CompileAdmission`, which is what
    /// makes a door that skips the gate a *compile error* rather than a red
    /// test. The `jit` crate's own tests are not doors — they hand the backend
    /// hand-built bytecode with no method identity to admit — so they need a
    /// way in, and an integration test under `jit/tests/` is a separate crate,
    /// so `#[cfg(test)]` cannot provide it.
    ///
    /// This is therefore a real bypass, and it is built so that using it in
    /// production is still caught: it does **not** open the thread scope, so a
    /// backend entry made under it is still counted by
    /// [`ungated_backend_entries`], which the VM asserts is zero. The type
    /// system stops the accident; the counter stops the deliberate misuse. The
    /// name is what makes the second one greppable.
    #[doc(hidden)]
    pub fn for_backend_test() -> Self {
        Self {
            door: CompileDoor::MethodEntry,
            _epoch: crate::open_compile_epoch_witness(),
            opened_scope: false,
            // Undeclared, like a door that never asked — so a test that hands
            // the backend direct calls is counted by
            // `undeclared_direct_bind_rows` exactly as a production bypass
            // would be. Same reasoning as `opened_scope: false`: the escape
            // hatch must not launder anything.
            direct_call_policy: std::sync::atomic::AtomicU8::new(DIRECT_POLICY_UNDECLARED),
            string_pin_asked: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Drop for CompileAdmission {
    fn drop(&mut self) {
        if self.opened_scope {
            // C1: charge the door that never put the String-intrinsic pin's
            // question. Doing it HERE rather than at the doors is the whole
            // point — a door that has not been taught to ask has, by
            // definition, no line of code in which to record that it did not.
            // The one thing every door does do is drop this token.
            //
            // Gated on `opened_scope` so `for_backend_test` cannot move it:
            // that keeps `asked + not-asked == admissions` an exact identity
            // per door, which is the property that makes the census readable.
            // (`opened_scope` is false only for that hatch, whose own doc
            // explains why it must not look like a real admission.)
            if !self
                .string_pin_asked
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                STRING_PIN_NOT_ASKED[self.door.index()].fetch_add(1, Ordering::Relaxed);
            }
            OPEN_ADMISSIONS.with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
}

/// The whole-method checks that must also veto *reusing* an already-compiled
/// body, not merely producing a new one.
///
/// Split out of [`admit`] because the OSR door consults a cached artifact
/// before deciding to compile, and a `CRATONVM_JIT_DENY` that stops the compile
/// while the cached body keeps running is exactly the lie
/// [`crate::jit_force_interpret`] documents. The resource and bail-list checks
/// deliberately do NOT live here: refusing to *reuse* a cached body because the
/// code cache is full would cost throughput and buy nothing — the body is
/// already committed.
pub fn compiled_execution_forbidden(class_name: &str, method_name: &str) -> bool {
    jit_disabled() || crate::jit_force_interpret(class_name, method_name)
}

/// `CRATONVM_DISABLE_JIT`, read through the shared flag snapshot.
///
/// Same source as the VM's `env_cache::disable_jit`, so the two cannot
/// disagree; the jit crate needs its own accessor because the gate lives below
/// the VM.
fn jit_disabled() -> bool {
    cratonvm_types::flags().jit.disable_jit
}

/// Ask the gate whether `door` may compile this method now.
///
/// On `Ok` the returned token must be held for the whole compilation.
pub fn admit(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    door: CompileDoor,
) -> Result<CompileAdmission, CompileRefusal> {
    // Open the compilation scope FIRST, before any constant-pool resolver runs.
    // Every `CompiledMethod` built under it — including one built by a nested
    // `callee_compiler` compile on this thread — is stamped with the install
    // epoch as of right now, so a redefinition or layout upgrade that lands
    // while this compile is reading bytecode makes the result unpublishable.
    // See `CompiledMethod::install_epoch` and `JitCache::flush_barrier`.
    //
    // Held even on the refusal paths below only for as long as this function
    // runs: the witness is dropped with the `Err`, which is correct — no
    // artifact was built.
    let epoch = crate::open_compile_epoch_witness();

    let refusal = if jit_disabled() {
        Some(CompileRefusal::JitDisabled)
    } else if crate::is_jit_bail_listed(class_name, method_name, descriptor) {
        crate::note_jit_bail_shortcircuit();
        Some(CompileRefusal::PermanentlyBailListed)
    } else if crate::jit_force_interpret(class_name, method_name) {
        Some(CompileRefusal::ForceInterpreted)
    } else if crate::jit_code_cache_at_capacity() {
        crate::note_jit_code_cache_cap_refusal();
        Some(CompileRefusal::CodeCacheAtCapacity)
    } else {
        None
    };

    if let Some(r) = refusal {
        REFUSALS[door.index()].fetch_add(1, Ordering::Relaxed);
        return Err(r);
    }

    // Same one-shot discipline the ordinary door has always had for the
    // bail-site record: clear whatever the previous compile on this thread
    // left behind, so a bail below can only ever report its own cause.
    let _ = crate::take_jit_bail_site();

    ADMISSIONS[door.index()].fetch_add(1, Ordering::Relaxed);
    OPEN_ADMISSIONS.with(|c| c.set(c.get() + 1));
    Ok(CompileAdmission {
        door,
        _epoch: epoch,
        opened_scope: true,
        // Undeclared until the door says otherwise. `admit`'s signature is
        // deliberately unchanged: adding a required parameter here would have
        // been the strongest possible enforcement — three production call
        // sites, two of them the very doors that need it — but two of those
        // three are in `vm/src/runtime/interpreter/**`, which this lane does
        // not own and could not build. H20-1 §6 specifies that change as O1;
        // this field is the half that could land green.
        direct_call_policy: std::sync::atomic::AtomicU8::new(DIRECT_POLICY_UNDECLARED),
        // Not asked until a door asks. `Drop` turns a token that is still
        // `false` here into a `string_pin_not_asked` for this door.
        string_pin_asked: std::sync::atomic::AtomicBool::new(false),
    })
}

/// Record that `n` direct-call rows reached the backend under `admission`.
///
/// Called once per backend entry from `x64::compile_with_param_slots`. The
/// second layer, in the shape this module's doc already argues for the backend
/// entry itself: *"The type system stops the accident; the counter stops the
/// deliberate misuse."* Here the type system cannot stop the accident at all —
/// see [`CompileAdmission::admits_direct_bind`] for why a refusal is only safe
/// at the bind site, one crate away — so for now the counter is the whole
/// instrument.
///
/// # H12-1 N2: this is the first counter that can see the OSR door's binds
///
/// `THREAD_CURRENT_THREAD_SITES_OSR` is the only per-door bind counter in the
/// tree that reaches the OSR door, and it covers one triple. `H7-1` §6c
/// predicted `collection_direct_helper_sites() == (0,0,0)` under `--jdk-only`
/// and treated a non-zero as its falsifier — but those counters live in
/// `try_compile_inner`'s ladder and are blind to the door where the HashMap
/// binds actually happen, so the prediction was confirmed by an instrument that
/// could not see the case it was about. This counts every row from every door.
pub fn note_direct_binds(admission: &CompileAdmission, n: usize) {
    if n == 0 {
        return;
    }
    if admission.direct_call_policy().is_none() {
        UNDECLARED_DIRECT_BIND_ROWS[admission.door.index()].fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Direct-call rows bound by `door` in compilations whose door never declared a
/// [`DirectCallPolicy`].
///
/// **Expected to reach zero at every door once H20-1's O1/O2 land, and to be
/// asserted zero from the VM thereafter.** Until then it is the size of the
/// bypass, per door, which no instrument in this tree reported before.
///
/// # Reading a zero
///
/// Ambiguous alone, for the reason `JIT_DIRECT_HELPER_JDK_ONLY_REFUSALS`' doc
/// gives: "no rows bound" and "every door declared" print the same `0`. Read it
/// beside [`admissions`] for the same door — a door with admissions and zero
/// undeclared rows has been taught; a door with no admissions has not run.
pub fn undeclared_direct_bind_rows(door: CompileDoor) -> u64 {
    UNDECLARED_DIRECT_BIND_ROWS[door.index()].load(Ordering::Relaxed)
}

/// Compilations at which `door` asked the String-intrinsic pin.
///
/// Read beside [`string_pin_not_asked`] and [`admissions`] for the same door;
/// the three partition exactly (`asked + not-asked == admissions`). A door
/// whose `asked` is `0` has not been taught the question, and every
/// process-global pin counter is blind to everything that door compiled —
/// which is the reading `fired=0` on `probes/CharAtWarmShape.java` was, and
/// which nothing in the tree could produce before.
pub fn string_pin_asked(door: CompileDoor) -> u64 {
    STRING_PIN_ASKED[door.index()].load(Ordering::Relaxed)
}

/// Compilations at which the pin, asked through `door`, kept the method off
/// the optimizing tier.
///
/// The per-door half of the census's global `fired`. A zero here means one of
/// two things and [`string_pin_asked`] separates them: `asked > 0` says the
/// pin looked and found nothing to pin, `asked == 0` says it was never
/// consulted at this door at all.
pub fn string_pin_declined(door: CompileDoor) -> u64 {
    STRING_PIN_DECLINED[door.index()].load(Ordering::Relaxed)
}

/// Compilations `door` was admitted for and never asked the pin about.
///
/// **The counter this lane exists for.** `fired=0` was read as "the pin is
/// correctly inert" when it meant "the traffic went through a door that never
/// asks"; those two are now different numbers. Expected to be the whole of the
/// OSR and eager-first-call rows until those doors are taught to ask (they are
/// in `vm/**`), and expected to fall at [`CompileDoor::MethodEntry`] to just
/// those compiles whose eligibility conjunction short-circuits before the pin
/// term — which is itself worth seeing, because a short-circuit is the other
/// way a pin term produces no census at all.
pub fn string_pin_not_asked(door: CompileDoor) -> u64 {
    STRING_PIN_NOT_ASKED[door.index()].load(Ordering::Relaxed)
}

/// Compiles admitted through `door`.
pub fn admissions(door: CompileDoor) -> u64 {
    ADMISSIONS[door.index()].load(Ordering::Relaxed)
}

/// Compiles refused at `door`.
pub fn refusals(door: CompileDoor) -> u64 {
    REFUSALS[door.index()].load(Ordering::Relaxed)
}

/// Whether an admission is open on the current thread.
pub fn admission_is_open() -> bool {
    OPEN_ADMISSIONS.with(|c| c.get()) > 0
}

/// Backend entries taken with no admission open on the thread.
///
/// **Expected to stay zero in the VM.** Non-zero means a door reached the
/// backend without the gate — the exact drift this module exists to prevent.
/// Non-zero inside the `jit` crate's own tests is normal: a unit test calling
/// the backend directly is not a door.
pub fn ungated_backend_entries() -> u64 {
    UNGATED_BACKEND_ENTRIES.load(Ordering::Relaxed)
}

/// Called by the backend on entry. See [`ungated_backend_entries`].
pub(crate) fn note_backend_entry() {
    if !admission_is_open() {
        UNGATED_BACKEND_ENTRIES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Test support: zero every counter.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    for d in CompileDoor::ALL {
        ADMISSIONS[d.index()].store(0, Ordering::Relaxed);
        REFUSALS[d.index()].store(0, Ordering::Relaxed);
        UNDECLARED_DIRECT_BIND_ROWS[d.index()].store(0, Ordering::Relaxed);
        STRING_PIN_ASKED[d.index()].store(0, Ordering::Relaxed);
        STRING_PIN_DECLINED[d.index()].store(0, Ordering::Relaxed);
        STRING_PIN_NOT_ASKED[d.index()].store(0, Ordering::Relaxed);
    }
    UNGATED_BACKEND_ENTRIES.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name no production class can collide with, so these tests can mark the
    /// bail-list without perturbing another test in the same binary.
    fn unique(tag: &str) -> String {
        format!("cratonvm/test/compile_gate/{tag}")
    }

    /// Serialises every test that reads a process-global counter, does
    /// something, and asserts the delta.
    ///
    /// `unique()` keeps the *bail-list* keys from colliding, but `ADMISSIONS`
    /// and `UNGATED_BACKEND_ENTRIES` are single counters shared by the whole
    /// binary: three tests here call `admit(.., CompileDoor::Osr)` and three
    /// call `note_backend_entry()`, so a read-then-assert pair in one sees
    /// another's increment and the `+1` becomes `+2`. It reproduced as
    /// `admissions_are_counted_per_door` failing in the full suite and passing
    /// in isolation — the shape that reads as flakiness and is not.
    ///
    /// A `>= before + 1` assertion would also make it pass, and would be
    /// strictly worse: the point of the counter is that it moves by exactly the
    /// number of admissions, and a `>=` cannot tell a double-count from a
    /// correct one.
    static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn an_admission_opens_and_closes_the_thread_scope() {
        let _guard = COUNTER_LOCK.lock();
        assert!(!admission_is_open());
        let cls = unique("scope");
        let a = admit(&cls, "m", "()V", CompileDoor::Osr).expect("clean method admits");
        assert!(admission_is_open());
        assert_eq!(a.door(), CompileDoor::Osr);
        drop(a);
        assert!(!admission_is_open());
    }

    /// The nesting case: `callee_compiler` compiles a callee inside the
    /// caller's compilation, so a flag would clear on the inner drop and make
    /// the rest of the outer compile read as ungated.
    #[test]
    fn admissions_nest() {
        let _guard = COUNTER_LOCK.lock();
        let outer =
            admit(&unique("nest-outer"), "m", "()V", CompileDoor::MethodEntry).expect("admits");
        let inner =
            admit(&unique("nest-inner"), "m", "()V", CompileDoor::MethodEntry).expect("admits");
        drop(inner);
        assert!(
            admission_is_open(),
            "the outer compilation is still open after the callee's admission drops"
        );
        drop(outer);
        assert!(!admission_is_open());
    }

    /// The check `compile_osr_artifact` hand-copied. A bail-listed method is
    /// refused at every door, not just the one that recorded the bail.
    #[test]
    fn a_bail_listed_method_is_refused_at_every_door() {
        let _guard = COUNTER_LOCK.lock();
        let cls = unique("bail-listed");
        crate::mark_jit_bail_listed(&cls, "m", "()V");
        for door in CompileDoor::ALL {
            assert_eq!(
                admit(&cls, "m", "()V", door).err(),
                Some(CompileRefusal::PermanentlyBailListed),
                "{} must honour the bail-list",
                door.label()
            );
        }
    }

    /// A permanent refusal and a transient one are different answers to
    /// "should I ask again", and the callers act on that difference.
    #[test]
    fn only_the_cap_refusal_is_transient() {
        assert!(CompileRefusal::CodeCacheAtCapacity.is_transient());
        assert!(!CompileRefusal::PermanentlyBailListed.is_transient());
        assert!(!CompileRefusal::ForceInterpreted.is_transient());
        assert!(!CompileRefusal::JitDisabled.is_transient());
    }

    /// The per-door counter is what makes "the OSR path calls the gate" a
    /// measurement. A gate whose counter did not move would let the VM-side
    /// assertion pass vacuously.
    #[test]
    fn admissions_are_counted_per_door() {
        let _guard = COUNTER_LOCK.lock();
        let before = admissions(CompileDoor::Osr);
        let _a = admit(&unique("counted"), "m", "()V", CompileDoor::Osr).expect("admits");
        assert_eq!(admissions(CompileDoor::Osr), before + 1);
    }

    /// The drift witness itself. Written as an explicit
    /// with-token/without-token pair because a counter that never moves and a
    /// counter that always moves both read as "zero ungated entries" from the
    /// VM side.
    #[test]
    fn the_ungated_witness_fires_only_without_a_token() {
        let _guard = COUNTER_LOCK.lock();
        let before = ungated_backend_entries();
        note_backend_entry();
        assert_eq!(
            ungated_backend_entries(),
            before + 1,
            "a backend entry with no admission open must be counted"
        );
        let _a = admit(&unique("witness"), "m", "()V", CompileDoor::Osr).expect("admits");
        note_backend_entry();
        assert_eq!(
            ungated_backend_entries(),
            before + 1,
            "a backend entry under an admission must not be counted"
        );
    }

    /// The escape hatch must not launder a backend entry.
    ///
    /// `for_backend_test` exists because an integration test under `jit/tests/`
    /// is a separate crate and cannot reach a `#[cfg(test)]` constructor. That
    /// makes it a real bypass of the type-level gate, so the *runtime* witness
    /// has to keep seeing through it — otherwise a production caller could
    /// reach for it and both layers would go quiet at once.
    #[test]
    fn the_backend_test_token_is_still_counted_as_ungated() {
        let _guard = COUNTER_LOCK.lock();
        let before = ungated_backend_entries();
        let t = CompileAdmission::for_backend_test();
        assert!(
            !admission_is_open(),
            "the test token must not open the thread scope"
        );
        note_backend_entry();
        assert_eq!(
            ungated_backend_entries(),
            before + 1,
            "a backend entry under the test token must still be counted"
        );
        drop(t);
        assert!(!admission_is_open());
    }

    /// A fresh admission has NOT declared a policy, and says so.
    ///
    /// Written as its own test because the whole instrument depends on
    /// "undeclared" being distinguishable from "compatible": `H12-1` §3b's
    /// sharpest fact is that `--jdk-only` and `--real-jdk` produced an
    /// identical bind profile, i.e. the door was answering "compatible" to a
    /// question nobody asked it.
    #[test]
    fn a_fresh_admission_has_not_declared_a_direct_call_policy() {
        let _guard = COUNTER_LOCK.lock();
        let a = admit(&unique("undeclared"), "m", "()V", CompileDoor::Osr).expect("admits");
        assert_eq!(a.direct_call_policy(), None);
        a.declare_direct_call_policy(DirectCallPolicy::JdkOnly);
        assert_eq!(a.direct_call_policy(), Some(DirectCallPolicy::JdkOnly));
        a.declare_direct_call_policy(DirectCallPolicy::Compatible);
        assert_eq!(a.direct_call_policy(), Some(DirectCallPolicy::Compatible));
    }

    /// The admission rule, in all three states, against both kinds.
    ///
    /// The `JdkOnly`/`Bridge` cell is the only `false` in the table, and it is
    /// the cell the OSR door was MEASURED getting wrong 298 000 times.
    #[test]
    fn the_direct_bind_rule_refuses_exactly_one_cell() {
        let _guard = COUNTER_LOCK.lock();
        let a = admit(&unique("rule"), "m", "()V", CompileDoor::Osr).expect("admits");

        // Undeclared admits both, deliberately — see `admits_direct_bind`.
        assert!(a.admits_direct_bind(|| true));
        assert!(a.admits_direct_bind(|| false));

        a.declare_direct_call_policy(DirectCallPolicy::Compatible);
        assert!(a.admits_direct_bind(|| true));
        assert!(
            a.admits_direct_bind(|| false),
            "compatible mode binds a Bridge shadow; that is what it is for"
        );

        a.declare_direct_call_policy(DirectCallPolicy::JdkOnly);
        assert!(
            a.admits_direct_bind(|| true),
            "a reviewed Intrinsic is 1.4's exception and still binds under strict"
        );
        assert!(
            !a.admits_direct_bind(|| false),
            "a Bridge shadow must defer to the real JDK bytecode under --jdk-only"
        );
    }

    /// `Compatible` must not be reachable by forgetting to answer.
    #[test]
    fn the_policy_witness_has_no_default() {
        assert_eq!(
            DirectCallPolicy::from_jdk_only(true),
            DirectCallPolicy::JdkOnly
        );
        assert_eq!(
            DirectCallPolicy::from_jdk_only(false),
            DirectCallPolicy::Compatible
        );
        // The rule, stated once, asserted here so a future edit to
        // `admits_shadow_bind` has to come past a test.
        assert!(DirectCallPolicy::Compatible.admits_shadow_bind(false));
        assert!(!DirectCallPolicy::JdkOnly.admits_shadow_bind(false));
    }

    /// The bypass witness: rows under an undeclared admission are counted, rows
    /// under a declared one are not.
    ///
    /// The with/without pair is written out for the reason
    /// `the_ungated_witness_fires_only_without_a_token` gives: a counter that
    /// never moves and one that always moves both read as "zero" from the VM.
    #[test]
    fn undeclared_direct_binds_are_counted_and_declared_ones_are_not() {
        let _guard = COUNTER_LOCK.lock();
        let before = undeclared_direct_bind_rows(CompileDoor::Osr);

        let a = admit(&unique("rows-undeclared"), "m", "()V", CompileDoor::Osr).expect("admits");
        note_direct_binds(&a, 7);
        assert_eq!(
            undeclared_direct_bind_rows(CompileDoor::Osr),
            before + 7,
            "a door that never declared a policy has its rows counted"
        );

        a.declare_direct_call_policy(DirectCallPolicy::JdkOnly);
        note_direct_binds(&a, 5);
        assert_eq!(
            undeclared_direct_bind_rows(CompileDoor::Osr),
            before + 7,
            "a door that declared is not counted"
        );

        // A compile with no direct calls must not move it either way.
        note_direct_binds(&a, 0);
        let b = admit(&unique("rows-empty"), "m", "()V", CompileDoor::Osr).expect("admits");
        note_direct_binds(&b, 0);
        assert_eq!(undeclared_direct_bind_rows(CompileDoor::Osr), before + 7);
    }

    /// Every door that builds a `direct_calls` plan owes the question, and
    /// names the ladder that owes it.
    ///
    /// The point is not the current answers — all three are `true` — but that
    /// `builds_direct_calls` is an exhaustive `match`, so a fourth door cannot
    /// be added without one. This test pins the ladder names beside them so a
    /// door whose ladder moves file is a red test rather than a stale doc.
    #[test]
    fn every_door_answers_the_direct_call_question() {
        for door in CompileDoor::ALL {
            assert!(
                door.builds_direct_calls(),
                "{} builds direct calls today; if that changed, change this test \
                 and say why in the record",
                door.label()
            );
            assert!(
                door.direct_call_ladder().contains(".rs"),
                "{} must name the file its ladder lives in",
                door.label()
            );
        }
        let mut ladders: Vec<&str> = CompileDoor::ALL
            .iter()
            .map(|d| d.direct_call_ladder())
            .collect();
        ladders.sort_unstable();
        ladders.dedup();
        assert_eq!(
            ladders.len(),
            CompileDoor::ALL.len(),
            "three doors, three distinct ladders — H12-2 1a's third reason a \
             grep could not produce the bind matrix"
        );
    }

    /// A door that never asks the String-intrinsic pin is COUNTED, by the
    /// token's `Drop`, against that door.
    ///
    /// This is the whole instrument. `fired=0` on the workload the pin governs
    /// was read as "correctly inert" and meant "compiled through a door that
    /// never asks"; nothing in the tree could tell those apart. Written as an
    /// explicit asked/not-asked pair for the reason
    /// `the_ungated_witness_fires_only_without_a_token` gives: a counter that
    /// never moves and one that always moves both read as a zero from outside.
    #[test]
    fn a_door_that_never_asks_the_string_pin_is_counted() {
        let _guard = COUNTER_LOCK.lock();
        let before_asked = string_pin_asked(CompileDoor::Osr);
        let before_not = string_pin_not_asked(CompileDoor::Osr);

        // A door that holds an admission and never puts the question.
        drop(admit(&unique("pin-silent"), "m", "()V", CompileDoor::Osr).expect("admits"));
        assert_eq!(string_pin_asked(CompileDoor::Osr), before_asked);
        assert_eq!(
            string_pin_not_asked(CompileDoor::Osr),
            before_not + 1,
            "a door that reached the backend without asking the pin must be a number"
        );

        // A door that asks. `invoke_ops` empty is the pin's `NoSite` — no
        // `invokevirtual`/`invokeinterface` could ever have been intrinsified —
        // so this asserts the ASKING, which is the fact the census was missing,
        // without depending on any env var or on a resolved String layout.
        let a = admit(&unique("pin-asks"), "m", "()V", CompileDoor::Osr).expect("admits");
        assert!(!a.string_intrinsic_pin_was_asked());
        assert!(!a.string_intrinsic_pin_declines(&[], None, None));
        assert!(a.string_intrinsic_pin_was_asked());
        assert_eq!(string_pin_asked(CompileDoor::Osr), before_asked + 1);
        // Asking twice must not double the ask count, or the partition
        // `asked + not-asked == admissions` stops holding.
        assert!(!a.string_intrinsic_pin_declines(&[], None, None));
        assert_eq!(string_pin_asked(CompileDoor::Osr), before_asked + 1);
        drop(a);
        assert_eq!(
            string_pin_not_asked(CompileDoor::Osr),
            before_not + 1,
            "a door that DID ask must not also be charged as silent"
        );
    }

    /// Every door answers whether it asks the pin, and today exactly one does.
    ///
    /// The point is not the answers but that `asks_string_intrinsic_pin` is an
    /// exhaustive `match`: a fourth door cannot be added without answering it.
    /// Same shape, and the same reason, as
    /// `every_door_answers_the_direct_call_question` above — that one was
    /// written after a door bound a helper nobody knew it bound; this one after
    /// a door compiled a whole population the pin never saw.
    #[test]
    fn every_door_asks_the_string_intrinsic_pin() {
        let asking: Vec<&str> = CompileDoor::ALL
            .iter()
            .filter(|d| d.asks_string_intrinsic_pin())
            .map(|d| d.label())
            .collect();
        assert_eq!(
            asking.len(),
            CompileDoor::ALL.len(),
            "a door stopped asking the String-intrinsic pin. This test was once \
             `exactly_one_door_asks_the_string_intrinsic_pin_today` and asserted \
             the OPPOSITE, because for a while exactly one door did: the pin \
             lived in `try_compile_inner`, which only `MethodEntry` reaches, and \
             the population that motivated the pin took the OSR door. That cost \
             five refuted hypotheses and was visible only as `fired=0` on the \
             very probe the pin exists for. If you are removing a door from this \
             set, the burden is to say why the counter it stops producing is not \
             the one somebody needs next. Asking: {asking:?}"
        );
    }

    #[test]
    fn every_door_has_a_distinct_slot_and_label() {
        let mut labels: Vec<&str> = CompileDoor::ALL.iter().map(|d| d.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), CompileDoor::ALL.len());
        let mut idx: Vec<usize> = CompileDoor::ALL.iter().map(|d| d.index()).collect();
        idx.sort_unstable();
        assert_eq!(idx, vec![0, 1, 2]);
    }
}
