// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class verification — Pass 2 (structural) and Pass 3 (bytecode).
//!
//! **Pass 2** validates class file structural integrity:
//! - Access flag validity (conflicting combinations)
//! - Final class constraint (cannot extend a final class)
//! - Final method constraint (cannot override a final method)
//! - Abstract method implementation (concrete classes must implement all abstract methods)
//! - Code attribute presence (non-abstract non-native must have Code; abstract/native must NOT)
//!
//! **Pass 3** performs bytecode type-checking using StackMapTable frames (JVM spec 4.10.1).
//! Requires Java 7+ (version >= 51). Delegated to [`super::bytecode_verifier`].
//!
//! ## Pre-Java-7 JSR/RET subroutines (JVMS §4.10.2.5)
//!
//! Class files compiled for major version ≤ 50 (Java 6 and earlier) may emit
//! `jsr` / `jsr_w` / `ret` instructions to implement `try-finally` blocks.
//! These were deprecated in Java 7 (which mandates `StackMapTable`) and are
//! formally specified in JVMS §4.10.2.5 to require **subroutine inlining** —
//! each `jsr` call site is verified independently with its own
//! `returnAddress` type, and the subroutine body is conceptually inlined per
//! call site. The naive type-inference verifier (which our
//! `verify_by_inference` worklist implements) cannot distinguish two `jsr`
//! call sites that target the same subroutine: when the worklist merges the
//! two incoming frames, the two distinct `ReturnAddress(pc1)` and
//! `ReturnAddress(pc2)` values on the operand stack collapse to `Top`,
//! causing the next `astore N` (the subroutine's "save the return address"
//! prologue) to fail with `astore: expected reference, found Top`.
//!
//! Real-world bytecode that hits this pattern: `javac -target 1.5/1.6` emits
//! a `try-finally` whose `finally` block becomes a subroutine, with one
//! `jsr` from the normal exit path and another `jsr` from each catch
//! handler. ByteBuddy 1.12 (compiled `--release 5`) ships several
//! `TypePool$AbstractBase$Hierarchical.clear` is one example.
//!
//! HotSpot's hand-written subroutine-inlining verifier handles these
//! correctly; reproducing it here is non-trivial (~1k LOC of bytecode
//! rewriting). Until that work lands, a method that contains
//! `jsr` / `jsr_w` / `ret` cannot be type-state-verified by our worklist
//! verifier at all.
//!
//! **SECURITY FIX (HIGH), version-gated policy.** An earlier revision routed
//! *every* subroutine-using method to a *structural-only* fallback (decode +
//! branch/handler bounds, but **no** operand-stack / local type-state check)
//! and accepted it. A permissive verifier is a memory-safety hole — but the
//! follow-on fix that *rejected every* subroutine-using method over-corrected:
//! it refused to load legal pre-Java-7 class files that HotSpot loads, which
//! regressed real applications (e.g. ByteBuddy 1.14's
//! `JavaDispatcher$DynamicClassLoader.proxy`, a Java 5 / major-49 class whose
//! `try-finally` compiles to a double-`jsr` subroutine).
//!
//! The policy is therefore gated on the **class-file version**, mirroring the
//! JVMS §4.9.1 static constraint (`jsr`/`jsr_w`/`ret` are legal at major ≤ 50,
//! forbidden at major ≥ 51):
//!   * **major ≤ 50 (Java 6 and earlier):** the opcodes are legal and HotSpot
//!     loads the class. CratonVM runs the structural sanity scan (so malformed
//!     bytecode is still caught) and then **accepts** — the same load decision
//!     HotSpot makes. The residual gap (no subroutine type-state check) is the
//!     same reduced guarantee already extended to trusted bootstrap classes,
//!     bounded to genuinely-legal bytecode, until a full §4.10.2.5 subroutine
//!     verifier lands (tracked follow-up).
//!   * **major ≥ 51 (Java 7+):** the opcodes are forbidden; the class is
//!     malformed. CratonVM **rejects** it with a hard `VerifyError`, matching
//!     HotSpot. (Java 7+ classes are otherwise required to ship `StackMapTable`
//!     and never legitimately emit `jsr`/`ret`.)
//!
//! The `CRATONVM_ALLOW_JSR_RET` opt-in escape hatch (see [`allow_jsr_ret`])
//! forces structural-only acceptance regardless of version, for deployments
//! that must load otherwise-malformed jars and accept the reduced guarantee.
//!
//! Methods that do **not** use subroutines are verified strictly via the
//! existing `bytecode_verifier::verify_bytecode` path, so the relaxation is
//! bounded to the exact set of methods that the worklist verifier cannot
//! model. Java 7+ classes that ship `StackMapTable` and no subroutines take
//! the fast path through `bytecode_verifier::verify_bytecode` unchanged.
//!
//! ## Type maps and the `jsr`/`ret` decision (arch-2026-07-26)
//!
//! [`super::type_maps`] retains what verification proves: a per-pc reference
//! layout of the local array and the operand stack, published per class into a
//! process-wide side table so the GC root scan and the interpreter fast path
//! can consume a *proof* instead of scanning conservatively. That recording
//! rides inside `bytecode_verifier::verify_bytecode`, which this module
//! **bypasses** for any class containing a subroutine-using method.
//!
//! Two things follow, and they are deliberately treated differently:
//!
//! 1. **Non-subroutine methods in a subroutine-bearing class were collateral
//!    damage.** One legacy `try-finally` in one method routed the *whole class*
//!    down [`verify_class_bytecode_inner`]'s per-method path, so every ordinary
//!    method in it lost its maps even though [`verify_method_typestate`] /
//!    [`verify_pre_java7_inference`] type-state-verify them exactly as
//!    `bytecode_verifier` would. Those two functions now build the same
//!    [`MethodTypeMapsBuilder`] and the class publishes a [`ClassTypeMaps`], so
//!    the loss is bounded to the subroutine methods themselves.
//!
//! 2. **Subroutine-using methods stay deliberately unproven — this is a
//!    decision, not an oversight.** They publish an *empty* map carrying
//!    [`FastPathVeto::Subroutine`] (see [`subroutine_unproven_maps`]): zero
//!    rows, so `oop_map_at` answers `None` at every pc (= "unproven, scan
//!    conservatively") and `safe_for_fast_path` is denied, but
//!    `fast_path_veto()` tells a consumer *why*, which distinguishes
//!    "deliberately conservative" from "class was never verified".
//!
//! The reason we do not simply run the existing worklist
//! ([`verify_pre_java7_inference`]) over subroutine methods and record its
//! result is that **the maps it would produce are unsound in both
//! directions**, which is strictly worse than no maps:
//!
//!   * **Missed roots.** JVMS §4.10.2.5 mandates per-call-site subroutine
//!     inlining precisely because a subroutine typically does *not* touch most
//!     of the caller's locals. A naive worklist merges all call sites at the
//!     subroutine entry, so a local that holds a reference on the path from
//!     `jsr` site A and an int on the path from `jsr` site B becomes
//!     `ref ⊔ int = Top` throughout the subroutine body — recorded as *not a
//!     reference*. Unlike an ordinary merge-to-`Top`, that slot is **not
//!     dead**: after `ret` control returns to site A, where the slot is read as
//!     a reference again. Failing to scan it is a missed root, i.e. heap
//!     corruption. The "unusable ⇒ dead ⇒ safe not to scan" argument that makes
//!     `Top` sound everywhere else does not hold across a `ret` edge.
//!   * **Non-references scanned as references.** The worklist does not even
//!     reach a fixpoint on these methods: the `astore` that saves the return
//!     address fails on the collapsed `Top` and the walk aborts. Rows written
//!     before the abort come from an unconverged state, so a slot can be
//!     recorded as a reference on the strength of one predecessor while another
//!     predecessor (not yet processed) supplies an int. Scanning an int as an
//!     oop is an immediate crash, not a conservative over-approximation.
//!
//! Closing this properly requires the real §4.10.2.5 subroutine-inlining
//! analysis (HotSpot does it in `GenerateOopMap`, not in its type checker) —
//! the same ~1k LOC of work the load-policy above already defers. Until then
//! the honest answer for these methods is `None`, and it is recorded as such.
//! The blast radius is bounded by JVMS §4.9.1: `jsr`/`ret` are illegal at
//! class-file major ≥ 51, so no class compiled for Java 7 (2011) or later can
//! reach this path at all.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::OnceLock;

use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::instruction::Instruction;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::stack_map::StackMapTable;
use cratonvm_reader::verified_code::{decode_instructions, DecodedInstructions};

use super::class::{find_method_recursive, Class, ClassStore};
use super::type_maps::{ClassTypeMaps, FastPathVeto, MethodTypeMaps, MethodTypeMapsBuilder};
use super::verify_frame::VerificationFrame;
use super::verify_insn::{verify_instruction_with, VerifyScratch};
use super::vtype::{descriptor_param_slots, param_types_from_descriptor, ClassHierarchy, VType};
use crate::loader_flags;
use cratonvm_types::error::LinkageError;

/// Verify a class: structural (Pass 2) + bytecode (Pass 3).
///
/// Called during the Loaded → Verified transition. Returns `LinkageError::VerifyError`
/// or `LinkageError::ClassFormatError` on failure.
pub fn verify_class(
    class: &Class,
    store: &ClassStore,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    verify_class_with_strictness(class, store, hierarchy, false)
}

/// [`verify_class`] with the `-Xverify:all` decision threaded in.
///
/// `strict = true` forces spec-literal branch-target checking on EVERY class,
/// including the trusted bootstrap image that [`verify_class`] verifies
/// leniently. This is the production entry point
/// `bytecode_verifier::verify_bytecode_strict` was documented as having and
/// did not: `ClassManager::strict_verification` (set from
/// `VmConfig::xverify_mode`) is what supplies the flag.
pub fn verify_class_with_strictness(
    class: &Class,
    store: &ClassStore,
    hierarchy: &dyn ClassHierarchy,
    strict: bool,
) -> Result<(), LinkageError> {
    verify_class_structure(class, store)?;
    verify_class_bytecode_with_strictness(class, hierarchy, strict)?;
    Ok(())
}

/// Pass 3 dispatcher with JSR/RET subroutine relaxation.
///
/// See module-level docs for the rationale. Common case (no `jsr`/`ret`
/// anywhere in the class) delegates to
/// [`super::bytecode_verifier::verify_bytecode`] verbatim. Otherwise,
/// each method is verified individually:
///   * methods using subroutines → structural sanity scan, then a
///     **version-gated** decision: a hard `VerifyError` when the class-file
///     version forbids the opcodes (major ≥ 51, JVMS §4.9.1 — the class is
///     malformed and HotSpot rejects it too), or structural-only acceptance
///     when the version permits them (major ≤ 50, where HotSpot loads the
///     class). Our worklist verifier collapses two distinct `ReturnAddress`
///     values into `Top` at a shared subroutine entry and so cannot
///     type-state-check legal subroutines yet; the `CRATONVM_ALLOW_JSR_RET`
///     escape hatch (see [`allow_jsr_ret`]) forces acceptance at any version;
///   * everything else → the same per-method type-state algorithm that
///     `bytecode_verifier::verify_bytecode` runs (StackMapTable-driven
///     for Java 7+, worklist-based for Java 6 and earlier).
///
/// The per-method routing isolates the decision: a non-JSR method in the same
/// class as a JSR method is still strictly type-state-verified, so a real bug
/// in the non-JSR method is neither masked by acceptance of a legal subroutine
/// nor by rejection of an illegal one.
///
/// This is the JSR-aware Pass 3 entry point; consumers that previously
/// called [`super::bytecode_verifier::verify_bytecode`] should call
/// this function instead for any class that may legitimately contain
/// pre-Java-7 subroutine bytecode (every classpath class with major
/// version ≤ 50 qualifies).
/// Cached verdict for the `CRATONVM_ALLOW_JSR_RET` escape hatch.
///
/// SECURITY FIX (HIGH): methods that use `jsr` / `jsr_w` / `ret` cannot be
/// type-state-verified by our worklist verifier (it collapses the two distinct
/// `ReturnAddress` values at a shared subroutine entry into `Top`; see the
/// module-level docs). The default policy is **version-gated** (JVMS §4.9.1):
/// such methods are accepted after their structural scan when the class-file
/// version legitimately permits the opcodes (major ≤ 50, matching HotSpot's
/// load decision) and rejected when the version forbids them (major ≥ 51, where
/// the class is malformed). The full §4.10.2.5 subroutine-inlining verifier is
/// non-trivial (~1k LOC of bytecode rewriting) and not yet implemented, so for
/// legal pre-Java-7 classes the structural decode/bounds checks run (to surface
/// malformed bytecode) but the type-state check inside the subroutine is
/// deferred to that follow-up.
///
/// Setting `CRATONVM_ALLOW_JSR_RET` to a non-empty, non-`"0"` value forces the
/// structural-only acceptance at **any** class-file version (including the
/// malformed major ≥ 51 case), for environments that must load such jars and
/// accept the reduced verification guarantee. Off by default. Read once at
/// process start (the env can't change mid-run), mirroring the `OnceLock`
/// pattern used elsewhere in this crate.
static ALLOW_JSR_RET: OnceLock<bool> = OnceLock::new();

/// `true` when `CRATONVM_ALLOW_JSR_RET` is set to a non-empty, non-`"0"`
/// value. Computed once and cached for the process lifetime. See
/// [`ALLOW_JSR_RET`].
fn allow_jsr_ret() -> bool {
    loader_flags().allow_jsr_ret
}

pub fn verify_class_bytecode(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    // The JSR/RET escape-hatch verdict is read once from the environment
    // (`CRATONVM_ALLOW_JSR_RET`); `false` (the default) means subroutine-using
    // methods are rejected after their structural scan. Threaded through the
    // inner function so tests can exercise both policies deterministically
    // without depending on the process-wide `OnceLock`.
    verify_class_bytecode_with_strictness(class, hierarchy, false)
}

/// [`verify_class_bytecode`] with the `-Xverify:all` decision threaded in.
pub fn verify_class_bytecode_with_strictness(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
    strict: bool,
) -> Result<(), LinkageError> {
    verify_class_bytecode_inner(class, hierarchy, allow_jsr_ret(), strict)
}

fn verify_class_bytecode_inner(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
    allow_jsr: bool,
    force_strict: bool,
) -> Result<(), LinkageError> {
    // Identify methods that use subroutines (jsr / jsr_w / ret). The scan
    // is opcode-only; we do not need to fully decode the bytecode to
    // detect these three opcodes (0xa8 / 0xc9 / 0xa9) and the `wide ret`
    // form (0xc4 0xa9). Variable-length operands (tableswitch /
    // lookupswitch) need to be skipped correctly so we do not mis-read a
    // jump-table padding byte as a JSR opcode.
    let mut any_jsr = false;
    // The canonical decode of each method the scan fetched, handed to the
    // verifier below so it is not fetched again (SBR round 5, lane verify).
    let mut decoded: Vec<Option<DecodedInstructions>> = Vec::with_capacity(class.methods.len());
    for method in class.methods.iter() {
        if method.is_abstract() || method.is_native() {
            decoded.push(None);
            continue;
        }
        let (uses_subroutines, decode) = method_uses_jsr_or_ret_decoded(method);
        decoded.push(decode);
        if uses_subroutines {
            any_jsr = true;
            break;
        }
    }

    if !any_jsr {
        // Common path — no subroutines anywhere. Delegate to the
        // standard verifier unchanged. `verify_bytecode` now self-selects
        // strict-by-default for untrusted classes (SECURITY FIX V4), and
        // `-Xverify:all` extends that to the trusted ones.
        return super::bytecode_verifier::verify_bytecode_decoded(
            class,
            hierarchy,
            force_strict,
            &mut decoded,
        );
    }

    // SECURITY FIX (V4): strict branch-target frame checking is the default
    // for untrusted (non-bootstrap-trusted) classes, consistent with
    // `bytecode_verifier::verify_bytecode`. The per-method JSR path below
    // honours the same decision so a JSR-containing untrusted class does not
    // silently downgrade its non-JSR sibling methods to lenient mode.
    let strict = force_strict || !super::bytecode_verifier::class_is_bootstrap_trusted(class);

    // POLICY: whether subroutine-using methods may be accepted under the
    // structural-only fallback. Two independent paths permit acceptance:
    //
    //   1. The class-file version is pre-Java-7 (major ≤ 50), where
    //      `jsr` / `jsr_w` / `ret` are **legal** bytecode (JVMS §4.9.1 forbids
    //      them only at version 51.0 and above). HotSpot loads such a class —
    //      its type-inference verifier applies §4.10.2.5 subroutine inlining.
    //      CratonVM's worklist verifier cannot yet model subroutine inlining
    //      (it collapses the two distinct `ReturnAddress` values at a shared
    //      subroutine entry into `Top`), so it falls back to the structural
    //      scan and accepts — matching the *load decision* HotSpot makes for
    //      the same class. The residual gap (no operand-stack/local type-state
    //      check inside the subroutine) is the same reduced guarantee CratonVM
    //      already extends to trusted bootstrap classes, and is bounded to
    //      genuinely-legal old bytecode. Full §4.10.2.5 subroutine type-state
    //      verification is tracked as follow-up work.
    //
    //   2. The `CRATONVM_ALLOW_JSR_RET` escape hatch is set, which forces
    //      structural-only acceptance regardless of version (for deployments
    //      that must load otherwise-malformed jars).
    //
    // A class-file version ≥ 51 that contains a subroutine opcode is
    // **malformed** — JVMS §4.9.1 forbids `jsr`/`jsr_w`/`ret` at 51.0+, so
    // HotSpot rejects it and so does CratonVM unless the escape hatch is set.
    // This is the SECURITY FIX (HIGH) boundary: the hard rejection is retained
    // exactly where the bytecode is illegal, and lifted where it is legal.
    let legal_pre_java7_subroutines = class.version.major <= ClassFileVersion::JAVA_6.major;
    let accept_subroutines = allow_jsr || legal_pre_java7_subroutines;

    // TYPE MAPS (arch-2026-07-26/classloading-verify-and-resolve): this path
    // is the reason subroutine-bearing classes used to answer `Unknown` to
    // every `type_maps_for` query — it never calls
    // `bytecode_verifier::verify_bytecode`, which is where the recording
    // lives. We now collect a per-method result here too, in `class.methods`
    // order (abstract/native methods contribute `None` so positions stay
    // aligned with `Class::methods`, which is what `ClassTypeMaps` indexes
    // by), and publish once the whole class has verified.
    //
    // Unconditional on the default path: no cargo feature, no `CRATONVM_*`
    // variable, no opt-in. The one class of methods that gets no rows is the
    // subroutine-using ones, and that is an explicit, reasoned `None` — see
    // the module docs and `subroutine_unproven_maps`.
    let mut collected: Vec<(Arc<str>, Arc<str>, Option<MethodTypeMaps>)> =
        Vec::with_capacity(class.methods.len());

    // At least one method in this class uses jsr/ret. Walk methods
    // individually so we can apply the structural-only fallback to the
    // subroutine-using ones while still type-state-verifying the rest.
    let mut scratch = VerifyScratch::for_class_version(class.version.major);
    for method in class.methods.iter() {
        if method.is_abstract() || method.is_native() {
            collected.push((method.name.clone(), method.descriptor.clone(), None));
            continue;
        }
        if method_uses_jsr_or_ret(method) {
            // Subroutine-using method. We always run the structural sanity scan
            // first so malformed bytecode (truncated instructions, out-of-range
            // jumps, ill-formed handler ranges) is rejected exactly as before,
            // regardless of the accept/reject decision below.
            // HotSpot's text for a structural fault (interpreter round i5
            // wave 103, lane L7; libverify's below major 50).
            if let Err(e) = verify_method_structural_only(class, method) {
                // Wave 104 hand-back (lane L7): the scan's subroutine-coverage
                // rule ("ret ... is not covered by a reachable jsr/astore
                // subroutine prologue") is this VM's own; libverify meets the
                // same shapes in its dataflow ("Recursive call to jsr entry",
                // "Multiple returns to single jsr"). Where the subroutine walk
                // runs, its refusal is HotSpot's.
                //
                // Wave 105 hand-back: and where the walk checks the whole
                // method and ACCEPTS it, HotSpot accepts it too (javac's
                // nested `finally`: a `jsr` to the inner subroutine inside
                // the outer one, whose `ret` the coverage rule misses), so
                // the coverage-only error no longer refuses; the method
                // goes on to the acceptance path below. The scan reports
                // coverage after every other structural check, so `e` being
                // the coverage error means nothing else failed. The scan's
                // error stays when the walk gives up (`Ok(false)`) or does
                // not run (`CRATONVM_VERIFY_SUBROUTINES=0`, trusted image,
                // major >= 51).
                let coverage = matches!(
                    &e,
                    LinkageError::VerifyError { message, .. }
                        if message.contains("is not covered by a reachable jsr/astore subroutine prologue")
                );
                let walk_accepted = coverage
                    && legal_pre_java7_subroutines
                    && strict
                    && loader_flags().verify_subroutines
                    && super::verify_subroutines::verify_subroutine_method(
                        class,
                        method,
                        hierarchy,
                        &mut scratch,
                    )?;
                if !walk_accepted {
                    return Err(
                        hotspot_structural_refusal(&class.name, method, class.version.major)
                            .unwrap_or(e),
                    );
                }
            }
            // SECURITY FIX (HIGH), version-gated: reject the type-verification
            // bypass only where the bytecode is *illegal* (class-file version
            // ≥ 51, where §4.9.1 forbids these opcodes). For legal pre-Java-7
            // versions — or under the `CRATONVM_ALLOW_JSR_RET` escape hatch —
            // accept the structurally-validated method, matching HotSpot's load
            // decision. See the `accept_subroutines` derivation above.
            if !accept_subroutines {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "method uses jsr/jsr_w/ret subroutine opcodes, which are forbidden in \
                         class-file version {}.0 and above (JVMS §4.9.1); refusing to load this \
                         malformed class (set CRATONVM_ALLOW_JSR_RET=1 to opt into legacy \
                         structural-only acceptance)",
                        class.version.major
                    ),
                });
            }
            // Interpreter round i5 wave 103, lane L7
            // (`i103-L7-jsr-methods-are-not-type-checked`): at a version where
            // the opcodes are legal HotSpot type-checks the method with
            // libverify's subroutine handling; so does this walk now
            // (`verify_subroutines`), for a class HotSpot verifies (not the
            // trusted boot image, not the escape hatch above 50).
            // `CRATONVM_VERIFY_SUBROUTINES=0` restores the structural-only
            // acceptance.
            if legal_pre_java7_subroutines && strict && loader_flags().verify_subroutines {
                super::verify_subroutines::verify_subroutine_method(
                    class,
                    method,
                    hierarchy,
                    &mut scratch,
                )?;
            }
            // OBSERVABILITY: this is a deliberate reduction in verification
            // strength, so it is stated out loud rather than inferred from the
            // absence of an error. `allow_jsr` names the escape hatch
            // (`CRATONVM_ALLOW_JSR_RET`) when that is what admitted the method,
            // as opposed to the version-gated default.
            tracing::warn!(
                class = %class.name,
                method = %method.name,
                descriptor = %method.descriptor,
                class_file_major = class.version.major,
                via_escape_hatch = allow_jsr && !legal_pre_java7_subroutines,
                "accepting a jsr/ret method on the structural-only path: its operand-stack \
                 and local type-state inside the subroutine is NOT verified (JVMS §4.10.2.5 \
                 subroutine inlining is not implemented)",
            );
            // TYPE MAPS: an explicit "proven nothing, and here is why" entry
            // rather than silence. Zero rows, so every `oop_map_at` answers
            // `None` (scan conservatively); `fast_path_veto()` reports
            // `Subroutine`. Fabricating an empty *bitmap* instead — i.e. rows
            // that claim "no references live here" — would be a missed-root
            // bug; see the module docs for why the worklist cannot be trusted
            // on these methods.
            collected.push((
                method.name.clone(),
                method.descriptor.clone(),
                Some(subroutine_unproven_maps(method)),
            ));
        } else {
            // Non-subroutine method: full type-state verification.
            // Performs the same algorithm as
            // `bytecode_verifier::verify_method` but in isolation per
            // method, so a real bug in this method cannot be masked by
            // a tolerated failure in a JSR-using method.
            //
            // TYPE MAPS: this walk is a real, complete type-state proof, so it
            // yields real maps — a legacy `finally` in a *sibling* method no
            // longer costs this one its precise oop maps.
            //
            // Interpreter round i5 wave 103, lane L7 (`i101-L7-verifyerror-
            // texts-are-not-hotspots`, item 3): this copy of the walk keeps
            // its own texts, so a refusal is re-diagnosed by the walk every
            // class without a subroutine gets (`bytecode_verifier::
            // verify_method`, HotSpot's split / libverify texts). Its error
            // is taken when it refuses too; the copy's otherwise.
            let maps =
                match verify_method_typestate(class, method, hierarchy, strict, &mut scratch, None) {
                    Ok(maps) => maps,
                    Err(err) => {
                        let quiet = NoVerifierLoads(hierarchy);
                        return Err(
                            super::bytecode_verifier::verify_method_for_refusal_text(
                                class, method, &quiet, strict,
                            )
                            .err()
                            .unwrap_or(err),
                        );
                    }
                };
            collected.push((method.name.clone(), method.descriptor.clone(), maps));
        }
    }

    // Publish only after every method verified: a class that fails
    // verification is never loaded, so half-built maps must not be visible.
    // `publish_class_type_maps` is a CAS install — if `verify_bytecode` also
    // ran for this class the first writer wins and this is a no-op.
    crate::type_maps::store_for(hierarchy).publish(class.id, ClassTypeMaps::new(collected));

    Ok(())
}

/// The deliberate "unproven" entry for a `jsr`/`ret` method.
///
/// Zero rows, so [`MethodTypeMaps::oop_map_at`] answers `None` at *every* pc —
/// which the type-map contract defines as "unproven, scan conservatively",
/// never as "no references live here". The
/// [`FastPathVeto::Subroutine`] tag is what makes the fallback diagnosable:
/// a consumer that finds `type_maps_for(..)` → `Some(m)` with
/// `m.fast_path_veto() == Some(FastPathVeto::Subroutine)` knows the conservative
/// answer is a recorded decision, not an unverified class.
///
/// See the module docs for why running the worklist over these methods and
/// recording *its* answer would be actively wrong rather than merely imprecise.
fn subroutine_unproven_maps(method: &ClassFileMethod) -> MethodTypeMaps {
    let (max_locals, max_stack) = match method.code() {
        Some(code) => (code.max_locals, code.max_stack),
        None => (0, 0),
    };
    let mut builder = MethodTypeMapsBuilder::new(max_locals, max_stack);
    builder.mark_unsafe_for_fast_path(FastPathVeto::Subroutine);
    // `finish(false)` would also veto with `IncompleteWalk`; the first veto
    // wins, so `Subroutine` — the informative one — is what survives.
    builder.finish(false)
}

/// The "walk did not complete" entry for a method whose Pass-3 walk returned
/// an error on the **map-collection** path.
///
/// Zero rows (`oop_map_at` → `None` at every pc → "unproven, scan
/// conservatively") plus the [`FastPathVeto::IncompleteWalk`] tag. Used only
/// by [`collect_class_type_maps`], where a walk failure is *not* a load
/// decision — see that function for why.
fn unproven_maps(method: &ClassFileMethod) -> MethodTypeMaps {
    let (max_locals, max_stack) = match method.code() {
        Some(code) => (code.max_locals, code.max_stack),
        None => (0, 0),
    };
    MethodTypeMapsBuilder::new(max_locals, max_stack).finish(false)
}

// ---------------------------------------------------------------------------
// Map collection without a load decision (arch-2026-07-26, coverage gap)
// ---------------------------------------------------------------------------

/// Run the Pass-3 walk **for its type maps only**, never rejecting the class.
///
/// # Why this exists
///
/// `class_manager::define_class_with_options` skips Pass 3 entirely for any
/// class defined by a *user-defined* loader while `loader_aware_resolution()`
/// is on (`defer_loader_sensitive_pass3`), and `vm::vm_util` deliberately does
/// not re-run it at link time. Both deferrals exist for one reason and one
/// reason only: the hierarchy adapter available at those points cannot always
/// keep two loaders' same-named classes apart, so its *assignability verdicts*
/// can be wrong and a wrong verdict there is a spurious `VerifyError` on
/// bytecode HotSpot accepts (the AssertJ `AbstractThrowableAssert.<init>`
/// case, 2026-07-13).
///
/// The consequence for type maps was total: every Spring / WildFly / H2 /
/// Elasticsearch application class is loaded by a user-defined loader, so
/// **none** of them had maps, `safe_for_fast_path` was unavailable for the
/// entire application workload, and the GC had no precise oop maps exactly
/// where the interesting object graphs live.
///
/// # Why publishing maps from an untrusted-verdict walk is sound
///
/// The two outputs of this walk are independent:
///
/// * The **verdict** (accept / `VerifyError`) is a function of
///   [`VerificationFrame::is_assignable_to`], which consults the hierarchy —
///   this is the part the deferral distrusts, and this function throws it
///   away rather than acting on it.
/// * The **rows** record, per pc, which local slots and which operand-stack
///   slots hold a *reference*. Reference-vs-primitive is decided by
///   [`VType::is_reference`] over types derived from field/method descriptors,
///   `ldc` constants, `new`/`anewarray` operands, and the class's own
///   `StackMapTable` — none of which consult the hierarchy. Merging two
///   reference types can pick the wrong common supertype under a confused
///   hierarchy, but the result is still a reference, so the *bit* is
///   unchanged. Operand-stack depths and local-slot indices are likewise
///   structural.
///
/// So a hierarchy that mis-resolves a name can make this walk accept bytecode
/// it should have rejected; it cannot make it record an int where a reference
/// lives or vice versa. That is exactly the property the maps' consumers (the
/// GC root scan, the interpreter fast path) depend on.
///
/// Anything the walk could not finish degrades honestly rather than silently:
/// a method whose walk errored gets [`unproven_maps`] (zero rows,
/// [`FastPathVeto::IncompleteWalk`]), a `jsr`/`ret` method gets
/// [`subroutine_unproven_maps`], and every unrecorded pc answers `None`, which
/// the type-map contract defines as "scan conservatively".
///
/// # Strictness
///
/// The walk runs **lenient** (`strict = false`). Strict mode's only extra
/// behaviour is to turn unreachable-code and frameless-branch-target findings
/// into a hard `VerifyError`; on this path that would throw away every row of
/// an otherwise perfectly describable method in exchange for an error nobody
/// acts on. Lenient keeps the reachable rows and denies `safe_for_fast_path`
/// via `IncompleteWalk`. This is not a downgrade of verification: **no**
/// verification decision is made here at all.
pub fn collect_class_type_maps(class: &Class, hierarchy: &dyn ClassHierarchy) -> ClassTypeMaps {
    let mut collected: Vec<(Arc<str>, Arc<str>, Option<MethodTypeMaps>)> =
        Vec::with_capacity(class.methods.len());
    // One constant pool for the whole class: resolved once per entry. The
    // JVMS §4.10.1.8 / §4.10.1.9 linkage rules are not applied here: this
    // walk makes no load decision, so a refusal would only cost the method
    // its maps (the deferred path enforces them in
    // `verify_deferred_class_bytecode`).
    let mut scratch = VerifyScratch::for_class_version(class.version.major).without_linkage_rules();

    for method in class.methods.iter() {
        // Abstract / native methods have no `Code`, but they still occupy an
        // index in `Class::methods`, which is what `ClassTypeMaps` is indexed
        // by — push `None` to keep the positions aligned.
        if method.is_abstract() || method.is_native() {
            collected.push((method.name.clone(), method.descriptor.clone(), None));
            continue;
        }
        let (uses_subroutines, decode) = method_uses_jsr_or_ret_decoded(method);
        if uses_subroutines {
            // Same reasoning as the verifying path: the worklist collapses two
            // distinct `ReturnAddress` values at a shared subroutine entry, so
            // recording *its* answer would be actively wrong. Record the
            // explicit "proven nothing, and here is why" entry instead.
            collected.push((
                method.name.clone(),
                method.descriptor.clone(),
                Some(subroutine_unproven_maps(method)),
            ));
            continue;
        }
        let maps =
            match verify_method_typestate(class, method, hierarchy, false, &mut scratch, decode) {
                Ok(maps) => maps,
                // The verdict is discarded (see the doc comment); only the fact
                // that the walk did not finish is retained.
                Err(_) => Some(unproven_maps(method)),
            };
        collected.push((method.name.clone(), method.descriptor.clone(), maps));
    }

    ClassTypeMaps::new(collected)
}

/// Collect and publish type maps for a class whose Pass 3 was deferred.
///
/// CAS install, first writer wins — so if the authoritative verifying path
/// ever does run for this class id, its maps are not clobbered by these.
/// Returns `true` if these maps were the ones installed.
pub fn publish_deferred_class_type_maps(class: &Class, hierarchy: &dyn ClassHierarchy) -> bool {
    crate::type_maps::store_for(hierarchy)
        .publish(class.id, collect_class_type_maps(class, hierarchy))
}

/// Re-collect and **replace** a class's type maps after its method bodies
/// changed (JVMTI `RedefineClasses` / `retransformClasses`, WP2.4).
///
/// This is not an optimisation. `publish_class_type_maps` is first-writer-wins,
/// so without an explicit replace the maps published when the class was first
/// defined would survive a redefine and go on describing the *old* method
/// bodies — at `Class::methods` indices the redefine may have reshuffled. A
/// stale oop map is a wrong oop map, which is heap corruption, so the maps must
/// be replaced in lockstep with the bytecode.
pub fn refresh_class_type_maps(class: &Class, hierarchy: &dyn ClassHierarchy) -> bool {
    crate::type_maps::store_for(hierarchy)
        .replace(class.id, collect_class_type_maps(class, hierarchy))
}

/// Single-method type-state verification mirroring the algorithm used by
/// `bytecode_verifier::verify_method`.
///
/// Branches on the class file version:
/// - **Java 7+ (major ≥ 51)**: requires `StackMapTable` for any method
///   with branches or exception handlers. Walks bytecode linearly,
///   adopting declared frames at branch targets and confirming the
///   current frame is assignable to each declared frame at the merge
///   points. Lenient mode — branch targets without declared frames
///   are tolerated (matches the existing
///   `bytecode_verifier::verify_bytecode` behaviour).
/// - **Pre-Java-7 (major ≤ 50)** without StackMapTable but with branches
///   or exception handlers: runs a worklist-based type inference.
/// - Pre-Java-7 with neither branches nor handlers: linear walk with
///   the initial frame.
///
/// Mirrors the invariants of `verify_method`. Imports the structural
/// out-of-range check from `verify_method_structural_only` because the
/// type-state path needs to reject malformed bytecode the same way.
///
/// TYPE MAPS: on success returns the [`MethodTypeMaps`] this walk proved.
/// `None` means there was nothing to describe (no `Code` attribute, or an
/// empty one), which consumers treat as "unproven".
fn verify_method_typestate(
    class: &Class,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
    // SECURITY FIX (V4): `true` for untrusted code — enforce spec-compliant
    // per-branch-target frame checking; `false` only for pre-verified trusted
    // bootstrap classes.
    strict: bool,
    // What this class's walks resolved from its constant pool so far.
    scratch: &mut VerifyScratch,
    // The method's decode, when the caller already made one (the subroutine
    // pre-scan does).
    pre_decoded: Option<DecodedInstructions>,
) -> Result<Option<MethodTypeMaps>, LinkageError> {
    // Structural sanity is a prerequisite for type-state verification:
    // we can't walk instructions if the bytecode itself is malformed. (The
    // decode it used is kept: the walk below reads its instructions instead of
    // decoding the bytes again.)
    let mut structural_decode: Option<DecodedInstructions> = pre_decoded;
    // HotSpot's text for the fault (interpreter round i5 wave 103, lane L7).
    verify_method_structural_shared(&class.name, method, &mut structural_decode).map_err(|e| {
        hotspot_structural_refusal(&class.name, method, class.version.major).unwrap_or(e)
    })?;

    let code_attr = match method.code() {
        Some(c) => c,
        None => return Ok(None),
    };
    let bytecode = &code_attr.code;
    if bytecode.is_empty() {
        return Ok(None);
    }

    let self_named = crate::bytecode_verifier::hidden_self_named_pool(class);
    let cp = self_named.as_ref().unwrap_or(&class.constant_pool);
    let class_name = &class.name;
    let version = &class.version;
    let requires_stack_map = version.major >= ClassFileVersion::JAVA_7.major;

    // Find the StackMapTable raw bytes among the Code attribute's
    // sub-attributes.
    // `entries: &Arc<[u8]>` (round 4 reader) — deref to a `&[u8]` slice
    // that lives as long as the attribute does.
    // Below major 50 HotSpot's class-file parser does not keep the table
    // (libverify never reads it; interpreter round i5 wave 109, lane L7).
    let stack_map_raw = code_attr
        .attributes
        .iter()
        .find_map(|a| match a {
            cratonvm_reader::attribute::Attribute::StackMapTable { entries } => Some(&entries[..]),
            _ => None,
        })
        .filter(|_| version.major >= ClassFileVersion::JAVA_6.major);

    if requires_stack_map && stack_map_raw.is_none() {
        let has_handlers = !code_attr.exception_table.is_empty();
        let has_branches = bytecode_has_any_branch(bytecode);
        if has_handlers || has_branches {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: "Java 7+ method with branches/exception handlers requires StackMapTable"
                    .to_string(),
            });
        }
    }

    if !requires_stack_map && stack_map_raw.is_none() {
        let has_handlers = !code_attr.exception_table.is_empty();
        let has_branches = bytecode_has_any_branch(bytecode);
        if has_handlers || has_branches {
            // Run the worklist-based pre-Java-7 type inference. We
            // re-use `bytecode_verifier::verify_bytecode` on a
            // single-method synthetic Class — but since `Class` is
            // not `Clone`able, we instead replicate the inference
            // worklist here. The algorithm is the same that lives in
            // `bytecode_verifier::verify_by_inference`.
            //
            // TYPE MAPS: that path serializes its settled fixpoint, which is
            // authoritative at every pc it covers (exception edges included),
            // so it needs no extra care here.
            return verify_pre_java7_inference(class, method, hierarchy, scratch).map(Some);
        }
        // No branches, no handlers — fall through to the linear walk
        // below.
    }

    let parsed_table = match stack_map_raw {
        Some(raw) => match StackMapTable::parse(raw) {
            Ok(table) => Some(table),
            Err(e) => {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!("failed to parse StackMapTable: {e}"),
                });
            }
        },
        None => None,
    };

    let (compact, initial) = VerificationFrame::initial_frames(
        class_name,
        &method.name,
        &method.descriptor,
        method.is_static(),
        code_attr.max_locals,
        code_attr.max_stack,
    );

    let declared_frames: rustc_hash::FxHashMap<u16, VerificationFrame> = match &parsed_table {
        Some(table) => {
            // Round 7 audit fix (MED #8): `absolute_offsets` now
            // returns `Result` so a malformed StackMapTable whose
            // accumulated offset overflows u16 surfaces as a verify
            // error here instead of silently wrapping.
            let offsets = table
                .absolute_offsets()
                .map_err(|e| LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!("StackMapTable absolute_offsets: {e}"),
                })?;
            // Frames are derived one from the previous, so build them in order
            // (the previous one is a borrow, not a clone) and move them into
            // the map afterwards; a repeated offset keeps the later frame.
            let mut built: Vec<(u16, VerificationFrame)> = Vec::with_capacity(table.entries.len());
            for (i, entry) in table.entries.iter().enumerate() {
                let off = offsets[i];
                let prev = match built.last() {
                    Some((_, frame)) => frame,
                    None => &compact,
                };
                let new_frame = prev.apply_stack_map_frame(entry, cp).map_err(|e| match e {
                    LinkageError::VerifyError { message, .. } => LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("StackMapTable frame {i} at offset {off}: {message}"),
                    },
                    other => other,
                })?;
                built.push((off, new_frame));
            }
            let mut frames = rustc_hash::FxHashMap::with_capacity_and_hasher(
                table.entries.len(),
                Default::default(),
            );
            for (off, frame) in built {
                frames.insert(off, frame);
            }
            frames
        }
        None => rustc_hash::FxHashMap::default(),
    };

    let mut handler_targets: rustc_hash::FxHashMap<u16, VType> =
        rustc_hash::FxHashMap::with_capacity_and_hasher(
            code_attr.exception_table.len(),
            Default::default(),
        );
    for entry in &code_attr.exception_table {
        let catch = super::bytecode_verifier::catch_type_of(entry, cp, class_name, &method.name)?;
        // HotSpot's `verify_exception_handler_table` checks each typed catch
        // type against `Throwable` before the method's code, which loads
        // both through the class's loader; record those loads in that order
        // (the subclass verdict itself is the VM's catch-type link step).
        if entry.catch_type != 0 && hierarchy.records_verifier_loads() {
            let throwable =
                VType::ObjectRef(crate::vtype::with_well_known(|w| Arc::clone(&w.throwable)));
            if catch != throwable {
                crate::vtype::note_hotspot_assignability_loads(&catch, &throwable, hierarchy);
            }
        }
        handler_targets.insert(entry.handler_pc, catch);
    }

    // Pre-Java-7 failover (JVMS §4.10.2) — mirrors
    // `bytecode_verifier::verify_method`. A major ≤ 50 class file may ship a
    // partial `StackMapTable`; where it does not cover a merge point the
    // linear walk cannot check that edge, so the worklist inference (which
    // merges at every branch target and handler entry) is the authority.
    let pre_java7_partial_frames = !requires_stack_map
        && parsed_table.is_some()
        && code_attr
            .exception_table
            .iter()
            .any(|e| !declared_frames.contains_key(&e.handler_pc));
    if pre_java7_partial_frames {
        return verify_pre_java7_inference(class, method, hierarchy, scratch).map(Some);
    }

    // SPEC-COMPLIANCE FIX (MED): map each `new` bytecode offset to the class
    // it creates, so `invokespecial <init>` can enforce the JVMS §4.10.1.9
    // owner-match (the `new`-site type must equal the constructor's owner).
    // `VType::Uninitialized(offset)` carries only the offset, so this side
    // table supplies the class name the type-state pass otherwise lacks.
    let mut new_site_classes: std::collections::HashMap<u16, std::sync::Arc<str>> =
        std::collections::HashMap::new();

    // TYPE MAPS: ride the walk below. Rows are recorded at instruction starts
    // with the type-state that holds immediately BEFORE the instruction
    // executes — after any declared StackMapTable frame or exception-handler
    // frame has been adopted, which is the state the interpreter and the GC
    // root scan actually observe at that pc.
    let mut type_maps = MethodTypeMapsBuilder::new(code_attr.max_locals, code_attr.max_stack);
    // `bytecode.len() / 2` is a floor on instruction density (the shortest
    // instruction is one byte, the average is ~2-3); `reserve` clamps it.
    type_maps.reserve(bytecode.len() / 2 + 1);
    // Cleared whenever the walk skips a region or reaches a pc whose
    // type-state it cannot vouch for; denies `safe_for_fast_path`.
    let mut walk_complete = true;

    let mut pc = 0usize;
    let mut current = initial;
    let mut verified = true;
    // TYPE MAPS: is `current` the real entry state for *this* pc?
    //
    // `verified` alone is not enough, and the difference matters for
    // soundness rather than precision. `verified` tracks only whether the
    // PREVIOUS instruction fell through; it is never re-armed when a declared
    // frame or a handler frame is adopted, and the unreachable-code guard
    // below is conditioned on `requires_stack_map`. So there are two pcs at
    // which `current` is a *stale* frame from an unrelated predecessor:
    //
    //   * pre-Java-7 classes that ship a `StackMapTable` (`requires_stack_map`
    //     is false, so the guard never fires) walking dead code after a
    //     `goto` / `return` / `athrow` that has no declared frame; and
    //   * a handler pc that is ALSO reachable by fall-through — the handler
    //     frame is installed only under `!verified`, so when control does fall
    //     through, `current` describes the fall-through state while the
    //     exception edge can enter the same pc with a different locals state
    //     and `[throwable]` on the stack. The two are never merged.
    //
    // Type-checking against a stale frame is a pre-existing laxity of this
    // walk. RECORDING one would be a different and much worse thing: a row
    // that claims a slot holds a reference when the other edge supplies an
    // int (an oop scan of a non-oop — an immediate crash), or claims it holds
    // none when the other edge supplies a live reference (a missed root).
    // So those pcs record nothing and the method loses `safe_for_fast_path`.
    let mut authoritative = true;
    // Offsets with a declared frame or a handler entry: the only pcs where the
    // two maps can answer, so every other pc skips their hash probes.
    let mut special_pcs = super::verify_frame::PcBits::new(bytecode.len());
    for &offset in declared_frames.keys() {
        special_pcs.insert(offset as usize);
    }
    for &offset in handler_targets.keys() {
        special_pcs.insert(offset as usize);
    }
    // The canonical decode's instructions (pcs ascend through the walk).
    let verified_insns: &[cratonvm_reader::verified_code::VerifiedInstruction] =
        match &structural_decode {
            Some(v) => v.instructions(),
            None => &[],
        };
    let mut insn_ix = 0usize;
    while pc < bytecode.len() {
        let is_special = special_pcs.contains(pc);
        if let Some(declared) = declared_frames.get(&(pc as u16)).filter(|_| is_special) {
            if verified && !current.is_assignable_to(declared, hierarchy) {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "frame mismatch at bytecode offset {pc}: \
                         current frame is not assignable to declared StackMapTable frame"
                    ),
                });
            }
            current.adopt_declared(declared, code_attr.max_locals);
            // A declared frame IS the authoritative merge-point state.
            authoritative = true;
        }

        if let Some(catch_type) = handler_targets.get(&(pc as u16)).filter(|_| is_special) {
            if !verified {
                // The handler frame is the current one with its stack replaced
                // (edited in place: it is discarded with the method on error).
                current.clear_stack();
                current
                    .push(catch_type.clone())
                    .map_err(|_| LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("exception handler stack overflow at offset {pc}"),
                    })?;
                authoritative = true;
            } else if declared_frames.get(&(pc as u16)).is_none() {
                // Handler entry that is also reachable by fall-through, with no
                // declared frame to reconcile the two. `current` describes only
                // the fall-through edge; the exception edge is unrepresented.
                // Record nothing here rather than a half-true row.
                authoritative = false;
            }
        }

        if !verified && requires_stack_map && !is_special {
            // SECURITY FIX (V4): in strict mode (untrusted code) unreachable
            // code with no declared frame is a VerifyError (JVMS §4.10.1).
            // Trusted bootstrap classes stay lenient and skip it silently.
            if strict {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "unreachable code at bytecode offset {pc}: \
                         no StackMapTable frame declared and control \
                         does not fall through"
                    ),
                });
            }
            // Lenient mode — skip unreachable code silently.
            //
            // TYPE MAPS: no row for a skipped pc (`oop_map_at` → `None` →
            // scan conservatively), and the method loses `safe_for_fast_path`
            // — the unchecked interpreter handlers need every executed
            // instruction proven, not merely most of them.
            walk_complete = false;
            let (_, next_pc) = match Instruction::decode(bytecode, pc) {
                Ok(r) => r,
                Err(_) => break,
            };
            pc = next_pc;
            continue;
        }

        // TYPE MAPS: capture the proven reference layout at this instruction
        // start. See the `authoritative` declaration for why this is guarded.
        if authoritative {
            type_maps.record(pc as u32, &current);
        } else {
            walk_complete = false;
        }

        // The instruction at `pc`: out of the canonical decode when `pc` is one
        // of its instruction starts (every pc the walk reaches is), else
        // decoded here as before.
        while insn_ix < verified_insns.len() && (verified_insns[insn_ix].pc as usize) < pc {
            insn_ix += 1;
        }
        let decoded_here: Instruction;
        let (insn, next_pc): (&Instruction, usize) = match verified_insns.get(insn_ix) {
            Some(v) if v.pc as usize == pc => (&v.instruction, v.next_pc as usize),
            _ => {
                let (decoded, next_pc) =
                    Instruction::decode(bytecode, pc).map_err(|e| LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("failed to decode instruction at offset {pc}: {e}"),
                    })?;
                decoded_here = decoded;
                (&decoded_here, next_pc)
            }
        };

        // TYPE MAPS: fold this instruction into the per-method fast-path
        // safety proof (local-slot operand bounds, jsr/ret, stray `wide`).
        type_maps.observe_instruction(insn);

        // Record the class created by a `new` so a later `invokespecial
        // <init>` on the resulting `Uninitialized(pc)` can be owner-matched.
        if let Instruction::New(idx) = insn {
            if let Some(created) = cp.get_class_name_arc(*idx) {
                new_site_classes.insert(pc as u16, created);
            }
        }
        // SPEC-COMPLIANCE FIX (MED): owner-match for `invokespecial <init>`
        // before the receiver is consumed (JVMS §4.10.1.9).
        check_new_init_owner_match(
            insn,
            &current,
            cp,
            &new_site_classes,
            class_name,
            &method.name,
        )?;

        let result = verify_instruction_with(
            insn,
            pc,
            &mut current,
            cp,
            class_name,
            &method.name,
            &method.descriptor,
            hierarchy,
            scratch,
        )
        .map_err(|e| match e {
            LinkageError::VerifyError {
                class_name: cn,
                method_name: mn,
                message,
            } => LinkageError::VerifyError {
                class_name: if cn.is_empty() {
                    class_name.to_string()
                } else {
                    cn
                },
                method_name: if mn.is_empty() {
                    method.name.to_string()
                } else {
                    mn
                },
                message: format!("at bytecode offset {pc}: {message}"),
            },
            other => other,
        })?;

        // SECURITY FIX (V4): in strict mode (untrusted code) every branch
        // target of this instruction must have a declared StackMapTable frame
        // (JVMS §4.10.1). A frameless target reached with a typed stack would
        // otherwise be accepted without a merge-point type check — the
        // type-confusion hole the lenient default left open. Trusted bootstrap
        // classes stay lenient (`strict == false`).
        if strict && requires_stack_map && parsed_table.is_some() {
            for &target in &result.branch_targets {
                if !declared_frames.contains_key(&target) {
                    return Err(LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!(
                            "strict verification: branch target at offset {target} \
                             has no StackMapTable frame"
                        ),
                    });
                }
            }
        }

        // Pre-Java-7 failover: a branch target with no declared frame means the
        // linear walk cannot check this edge. Hand the method to the §4.10.2
        // worklist, whose verdict is merge-checked. See the derivation of
        // `pre_java7_partial_frames` above.
        if !requires_stack_map {
            for &target in &result.branch_targets {
                if !declared_frames.contains_key(&target) {
                    return verify_pre_java7_inference(class, method, hierarchy, scratch).map(Some);
                }
            }
        }

        verified = result.falls_through;
        if !result.falls_through && next_pc < bytecode.len() {
            verified = false;
        }
        // TYPE MAPS: the next pc inherits an authoritative frame only when
        // control actually flows into it from here. Otherwise it needs a
        // declared frame or a handler frame to re-arm (see the loop head).
        authoritative = authoritative && result.falls_through && next_pc < bytecode.len();
        pc = next_pc;
    }

    Ok(Some(type_maps.finish(walk_complete)))
}

/// Pre-Java-7 worklist type inference for a single method.
///
/// Mirrors `bytecode_verifier::verify_by_inference`. Used only for
/// non-JSR pre-Java-7 methods (subroutine-using methods take the
/// structural-only fallback before reaching here).
///
/// TYPE MAPS: when the worklist settles, `frame_at` *is* the answer — it maps
/// every reachable instruction start to its fixpoint entry type-state,
/// including exception-handler entries (they are merged in below, unlike the
/// linear StackMapTable walk). The maps are serialized from that map, so this
/// path performs no extra dataflow; it only writes down dataflow it already
/// finished. Every pc the worklist did not reach is simply absent
/// (`oop_map_at` → `None` → scan conservatively).
fn verify_pre_java7_inference(
    class: &Class,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
    // The class's scratch: carries the old-verifier message choice and the
    // linkage-rule switches (`VerifyScratch`).
    scratch: &mut VerifyScratch,
) -> Result<MethodTypeMaps, LinkageError> {
    let code_attr = match method.code() {
        Some(c) => c,
        None => {
            // No body to describe. An empty builder yields zero rows, so every
            // pc answers `None` — the same conservative answer, minus the
            // special case in the caller.
            return Ok(MethodTypeMapsBuilder::new(0, 0).finish(false));
        }
    };
    let bytecode = &code_attr.code;
    let self_named = crate::bytecode_verifier::hidden_self_named_pool(class);
    let cp = self_named.as_ref().unwrap_or(&class.constant_pool);
    let class_name = &class.name;

    let initial = VerificationFrame::initial_frame(
        class_name,
        &method.name,
        &method.descriptor,
        method.is_static(),
        code_attr.max_locals,
        code_attr.max_stack,
    );

    let mut frame_at: std::collections::HashMap<usize, VerificationFrame> =
        std::collections::HashMap::with_capacity(32);
    frame_at.insert(0, initial);

    let mut worklist: Vec<usize> = vec![0];
    let mut enqueued: std::collections::HashSet<usize> = std::collections::HashSet::new();
    enqueued.insert(0);

    // SPEC-COMPLIANCE FIX (MED): `new`-offset → created-class side table for
    // the `invokespecial <init>` owner-match (JVMS §4.10.1.9). Accumulated
    // across the worklist run; an `Uninitialized(offset)` receiver can only
    // reach an `<init>` after the dominating `new` at `offset` was decoded, so
    // the entry is present by the time the check runs.
    let mut new_site_classes: std::collections::HashMap<u16, std::sync::Arc<str>> =
        std::collections::HashMap::new();

    let max_iterations = bytecode.len().saturating_mul(4).max(256);
    let mut iterations = 0usize;

    while let Some(pc) = worklist.pop() {
        enqueued.remove(&pc);
        iterations += 1;
        if iterations > max_iterations {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: "type inference exceeded iteration limit".to_string(),
            });
        }
        if pc >= bytecode.len() {
            continue;
        }

        let mut current = match frame_at.get(&pc) {
            Some(f) => f.clone(),
            None => continue,
        };

        let (insn, next_pc) =
            Instruction::decode(bytecode, pc).map_err(|e| LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!("failed to decode instruction at offset {pc}: {e}"),
            })?;

        // Record the class created by a `new` so a later `invokespecial
        // <init>` on the resulting `Uninitialized(pc)` can be owner-matched.
        if let Instruction::New(idx) = &insn {
            if let Some(created) = cp.get_class_name_arc(*idx) {
                new_site_classes.insert(pc as u16, created);
            }
        }
        // SPEC-COMPLIANCE FIX (MED): owner-match for `invokespecial <init>`
        // before the receiver is consumed (JVMS §4.10.1.9).
        check_new_init_owner_match(
            &insn,
            &current,
            cp,
            &new_site_classes,
            class_name,
            &method.name,
        )?;

        let result = verify_instruction_with(
            &insn,
            pc,
            &mut current,
            cp,
            class_name,
            &method.name,
            &method.descriptor,
            hierarchy,
            scratch,
        )
        .map_err(|e| match e {
            LinkageError::VerifyError {
                class_name: cn,
                method_name: mn,
                message,
            } => LinkageError::VerifyError {
                class_name: if cn.is_empty() {
                    class_name.to_string()
                } else {
                    cn
                },
                method_name: if mn.is_empty() {
                    method.name.to_string()
                } else {
                    mn
                },
                message: format!("at bytecode offset {pc}: {message}"),
            },
            other => other,
        })?;

        if result.falls_through && next_pc < bytecode.len() {
            if merge_frame_into(
                &mut frame_at,
                next_pc,
                &current,
                hierarchy,
                class_name,
                &method.name,
            )? {
                if enqueued.insert(next_pc) {
                    worklist.push(next_pc);
                }
            }
        }

        for &target in &result.branch_targets {
            let target_pc = target as usize;
            if target_pc >= bytecode.len() {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "branch target {target_pc} at offset {pc} is out of range \
                         (bytecode length = {})",
                        bytecode.len()
                    ),
                });
            }
            if merge_frame_into(
                &mut frame_at,
                target_pc,
                &current,
                hierarchy,
                class_name,
                &method.name,
            )? {
                if enqueued.insert(target_pc) {
                    worklist.push(target_pc);
                }
            }
        }

        for entry in &code_attr.exception_table {
            if pc >= entry.start_pc as usize && pc < entry.end_pc as usize {
                let catch =
                    super::bytecode_verifier::catch_type_of(entry, cp, class_name, &method.name)?;
                let mut handler_frame = current.clone();
                handler_frame.clear_stack();
                handler_frame
                    .push(catch)
                    .map_err(|_| LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!(
                            "exception handler stack overflow at handler pc {}",
                            entry.handler_pc
                        ),
                    })?;
                let handler_pc = entry.handler_pc as usize;
                if merge_frame_into(
                    &mut frame_at,
                    handler_pc,
                    &handler_frame,
                    hierarchy,
                    class_name,
                    &method.name,
                )? {
                    if enqueued.insert(handler_pc) {
                        worklist.push(handler_pc);
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // TYPE MAPS: serialize the settled fixpoint.
    // -----------------------------------------------------------------------
    //
    // `frame_at[pc]` is the merged type-state on ENTRY to the instruction at
    // `pc` — exactly what the interpreter and the GC root scan observe when a
    // frame's pc is `pc`. Rows must be emitted in ascending pc order because
    // `MethodTypeMaps` indexes them with a binary search.
    let mut type_maps = MethodTypeMapsBuilder::new(code_attr.max_locals, code_attr.max_stack);
    type_maps.reserve(frame_at.len());
    let mut ordered: Vec<usize> = frame_at.keys().copied().collect();
    ordered.sort_unstable();

    // Coverage is whatever the worklist reached. Anything it did not reach is
    // absent from the map (`oop_map_at` → `None` → conservative), and a decode
    // failure at a recorded pc denies `safe_for_fast_path`.
    let mut walk_complete = true;
    for pc in ordered {
        let Some(frame) = frame_at.get(&pc) else {
            continue;
        };
        type_maps.record(pc as u32, frame);
        match Instruction::decode(bytecode, pc) {
            Ok((insn, _)) => type_maps.observe_instruction(&insn),
            Err(_) => walk_complete = false,
        }
    }

    Ok(type_maps.finish(walk_complete))
}

/// Merge `incoming` into the frame at `target_pc` in the frame map.
/// Returns `true` if the target's frame changed.
fn merge_frame_into(
    frame_at: &mut std::collections::HashMap<usize, VerificationFrame>,
    target_pc: usize,
    incoming: &VerificationFrame,
    hierarchy: &dyn ClassHierarchy,
    class_name: &str,
    method_name: &str,
) -> Result<bool, LinkageError> {
    match frame_at.get(&target_pc) {
        None => {
            frame_at.insert(target_pc, incoming.clone());
            Ok(true)
        }
        Some(existing) => {
            if incoming.is_assignable_to(existing, hierarchy) {
                Ok(false)
            } else {
                let merged = existing.merge(incoming, hierarchy).map_err(|e| match e {
                    LinkageError::VerifyError { message, .. } => LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method_name.to_string(),
                        message: format!("frame merge at offset {target_pc}: {message}"),
                    },
                    other => other,
                })?;
                let changed = !merged.is_assignable_to(existing, hierarchy)
                    || !existing.is_assignable_to(&merged, hierarchy);
                frame_at.insert(target_pc, merged);
                Ok(changed)
            }
        }
    }
}

/// Quick scan for any branch instruction (mirrors
/// `bytecode_verifier::bytecode_has_branches`).
fn bytecode_has_any_branch(bytecode: &[u8]) -> bool {
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let opcode = bytecode[pc];
        match opcode {
            0x99..=0xa6 | 0xa7 | 0xa8 | 0xaa | 0xab | 0xc6 | 0xc7 | 0xc8 | 0xc9 => return true,
            0xc4 => {
                pc += if pc + 1 < bytecode.len() && bytecode[pc + 1] == 0x84 {
                    6
                } else {
                    4
                };
                continue;
            }
            _ => {}
        }
        // Use the canonical decoder to step over variable-length
        // instructions (tableswitch/lookupswitch).
        match Instruction::decode(bytecode, pc) {
            Ok((_, next)) if next > pc => pc = next,
            _ => return false,
        }
    }
    false
}

/// Scan a method's bytecode for `jsr` / `jsr_w` / `ret` (incl. `wide ret`).
///
/// Returns `true` if any subroutine-related opcode is present. We walk
/// instruction-by-instruction so that variable-length opcodes
/// (`tableswitch` / `lookupswitch`) and aligned padding are stepped over
/// correctly — otherwise a jump-table byte that happens to be `0xa8`
/// would be misread as `jsr`.
pub(crate) fn method_uses_jsr_or_ret(method: &ClassFileMethod) -> bool {
    method_uses_jsr_or_ret_decoded(method).0
}

/// [`method_uses_jsr_or_ret`] that also returns the canonical decode it
/// consulted (`None` when the method has no code or the decode failed).
fn method_uses_jsr_or_ret_decoded(method: &ClassFileMethod) -> (bool, Option<DecodedInstructions>) {
    let code_attr = match method.code() {
        Some(c) => c,
        None => return (false, None),
    };
    let bytecode = &code_attr.code;
    // SBR round 5, lane warmup: ask the canonical decode the verifier needs a
    // moment later anyway, instead of decoding every instruction a second time
    // here. `jsr`, `jsr_w` and `ret` (the `wide ret` form decodes to `Ret` too)
    // are exactly the three opcodes the scan below looks for. A body the
    // canonical decode rejects takes the scan, which answers for the
    // instructions before the bad one, as it always did. (Lane verify: the
    // decode is handed to the caller, which walks it, instead of being looked
    // up again in the process cache.)
    if let Ok(decoded) = decode_instructions(bytecode) {
        let uses = decoded.instructions().iter().any(|d| {
            matches!(
                d.instruction,
                Instruction::Jsr(_) | Instruction::JsrW(_) | Instruction::Ret(_)
            )
        });
        return (uses, Some(decoded));
    }
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let opcode = bytecode[pc];
        // 0xa8 = jsr, 0xa9 = ret, 0xc9 = jsr_w
        if opcode == 0xa8 || opcode == 0xa9 || opcode == 0xc9 {
            return (true, None);
        }
        // wide prefix may wrap a `ret` (0xc4 0xa9 ...): catch that too.
        if opcode == 0xc4 && pc + 1 < bytecode.len() && bytecode[pc + 1] == 0xa9 {
            return (true, None);
        }
        // Use the canonical decoder to advance. Malformed bytecode here
        // is harmless — `verify_method_structural_only` will reject it.
        let next = match Instruction::decode(bytecode, pc) {
            Ok((_, next_pc)) => next_pc,
            Err(_) => return (false, None),
        };
        if next <= pc {
            // defensive: zero/negative advance — stop scanning
            return (false, None);
        }
        pc = next;
    }
    (false, None)
}

/// Structural sanity scan for a method's bytecode (JVMS §4.9.1).
///
/// Validates:
///   - every byte of `code` decodes into a legal `Instruction`;
///   - every branch target lies inside `code`;
///   - every entry of the exception table satisfies
///     `0 ≤ start_pc < end_pc ≤ code_length` and `handler_pc < code_length`;
///   - `max_stack` and `max_locals` are well-formed (non-zero code).
///
/// This is JVMS §4.9.1 "Static Constraints" — the structural envelope
/// that a class file must satisfy regardless of the type-state pass
/// (§4.10). It catches the malformed-bytecode shapes the runtime
/// interpreter cannot defend against (out-of-range jumps, truncated
/// instructions) without requiring a working type-state model — which is
/// what makes it the right fallback for `jsr`/`ret` methods.
#[derive(Debug, Clone)]
struct StructuralInstruction {
    pc: usize,
    next_pc: usize,
    insn: Instruction,
}

/// A borrowed view of one decoded instruction (the hot structural scan reads
/// the verified decode in place instead of cloning it into a
/// [`StructuralInstruction`]).
struct DecodedRef<'a> {
    pc: usize,
    next_pc: usize,
    insn: &'a Instruction,
}

/// The instruction start offsets of one method as a dense bitmap (a method's
/// code is at most 65535 bytes). Replaces a per-method `HashSet<usize>`.
struct PcSet {
    bits: super::verify_frame::PcBits,
}

impl PcSet {
    fn from_starts(code_len: usize, starts: impl Iterator<Item = u32>) -> Self {
        let mut bits = super::verify_frame::PcBits::new(code_len);
        for pc in starts {
            bits.insert(pc as usize);
        }
        Self { bits }
    }

    #[inline]
    fn contains(&self, pc: &usize) -> bool {
        self.bits.contains(*pc)
    }
}

#[derive(Debug, Clone, Copy)]
struct JsrSite {
    target: usize,
}

/// Decode-only structural validation used before type-state verification and
/// as the bounded fallback for legal legacy jsr/ret methods.
fn verify_method_structural_only(
    class: &Class,
    method: &ClassFileMethod,
) -> Result<(), LinkageError> {
    verify_method_structural(&class.name, method)
}

/// Run the JVMS §4.9.1 structural scan over every method of `class`.
///
/// This is the **hierarchy-independent** half of bytecode verification:
/// instruction decoding, branch/handler bounds and instruction-boundary
/// landing, local-index and `max_locals` conformance, `jsr`/`ret` shape. None
/// of it consults [`ClassHierarchy`], so it can be run on the paths that
/// deliberately defer the *type-state* verdict because their hierarchy adapter
/// cannot tell two loaders' same-named classes apart (see
/// [`collect_class_type_maps`] and `class_manager`'s
/// `defer_loader_sensitive_pass3`). Those paths previously made **no** load
/// decision at all, so malformed bytecode from a user-defined loader — an
/// out-of-range branch, a handler entry in the middle of an instruction —
/// reached the interpreter unchallenged.
///
/// Fails closed: the first malformed method aborts the class with a
/// `VerifyError` naming the class, the method and the rule.
pub fn verify_class_structural_bytecode(class: &Class) -> Result<(), LinkageError> {
    for method in class.methods.iter() {
        if method.is_abstract() || method.is_native() {
            continue;
        }
        verify_method_structural(&class.name, method)?;
    }
    Ok(())
}

/// What a class whose Pass-3 type-state verdict is deferred
/// (`ClassManager::define_class_with_options`, `defer_loader_sensitive_pass3`:
/// every class a user-defined loader defines) is still held to:
/// [`verify_class_structural_bytecode`], then the two linkage rules HotSpot's
/// verifier applies to every class such a loader defines and that need no
/// distrusted assignability verdict to fire:
///
/// * JVMS §4.10.1.8, the protected-receiver rule (`getfield` / `putfield` /
///   `invokevirtual` of a protected member of a proper superclass in another
///   run-time package, through an objectref not assignable to the current
///   class): "Bad access to protected data in getfield|invokevirtual";
/// * JVMS §4.10.1.9, the `invokespecial` target rule: "Bad invokespecial
///   instruction: current class isn't assignable to reference class." /
///   "... interface method reference is in an indirect superinterface.".
///
/// Both answers come from the class's LINKED supertypes (by `ClassId`, not by
/// name lookup) and the loaded member flags, which the loader-confusion the
/// deferral guards against cannot falsify; an unloaded receiver class is
/// answered "assignable" (the hierarchy's optimism), so the rule never refuses
/// on a class it cannot see.
///
/// The type-state walk runs only for a method with an instruction the rules
/// could refuse ([`super::verify_insn::linkage_rule_candidate`], decided
/// from the constant pool and the supertypes alone), lenient, without
/// recording verifier loads, and only a linkage-rule refusal is acted on:
/// any other walk error is the distrusted verdict and is discarded, as
/// before. A `jsr`/`ret` method is not walked (the worklist cannot model it).
pub fn verify_deferred_class_bytecode(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    let mut scratch: Option<VerifyScratch> = None;
    let old_verifier = class.version.major <= 50;
    let self_named = crate::bytecode_verifier::hidden_self_named_pool(class);
    let cp = self_named.as_ref().unwrap_or(&class.constant_pool);
    for method in class.methods.iter() {
        if method.is_abstract() || method.is_native() {
            continue;
        }
        let mut decode: Option<DecodedInstructions> = None;
        verify_method_structural_shared(&class.name, method, &mut decode)?;
        let Some(decoded) = decode else {
            continue;
        };
        let mut uses_subroutines = false;
        let mut candidate = false;
        for d in decoded.instructions() {
            if matches!(
                d.instruction,
                Instruction::Jsr(_) | Instruction::JsrW(_) | Instruction::Ret(_)
            ) {
                uses_subroutines = true;
                break;
            }
            if !candidate {
                candidate = super::verify_insn::linkage_rule_candidate(
                    &d.instruction,
                    cp,
                    &class.name,
                    hierarchy,
                    old_verifier,
                );
            }
        }
        if uses_subroutines || !candidate {
            continue;
        }
        let scratch =
            scratch.get_or_insert_with(|| VerifyScratch::for_class_version(class.version.major));
        let quiet = NoVerifierLoads(hierarchy);
        if let Err(err) =
            verify_method_typestate(class, method, &quiet, false, scratch, Some(decoded))
        {
            if scratch.take_linkage_rule_violation() {
                return Err(linkage_rule_refusal_text(class, method, &quiet, err));
            }
        }
    }
    Ok(())
}

/// HotSpot's text for a linkage-rule refusal `err` of the deferred walk
/// (interpreter round i5 wave 109, lane L7): that walk's copy words it as
/// "at bytecode offset N: <rule text>", so the refusal is re-diagnosed by
/// `bytecode_verifier::verify_method` (split `Exception Details`, libverify's
/// `(class: ..)` text), and that answer is taken only when it is the same
/// rule (its message carries the rule's text; libverify's shorter
/// "Bad access to protected data" counts) -- an earlier, distrusted refusal
/// of the full walk is never substituted.
fn linkage_rule_refusal_text(
    class: &Class,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
    err: LinkageError,
) -> LinkageError {
    let rule = match &err {
        LinkageError::VerifyError { message, .. } => message
            .split_once(": ")
            .map(|(_, rule)| rule)
            .unwrap_or(message.as_str())
            .lines()
            .next()
            .unwrap_or("")
            .to_string(),
        _ => return err,
    };
    let rule_core = rule
        .strip_suffix(" in getfield")
        .or_else(|| rule.strip_suffix(" in putfield"))
        .or_else(|| rule.strip_suffix(" in invokevirtual"))
        .unwrap_or(rule.as_str());
    match super::bytecode_verifier::verify_method_for_refusal_text(class, method, hierarchy, false) {
        Err(LinkageError::VerifyError {
            class_name,
            method_name,
            message,
        }) if !rule_core.is_empty()
            && (message.contains(rule_core)
                || (rule_core.starts_with("Bad invokespecial instruction")
                    && message.contains("Illegal use of nonvirtual function call"))) =>
        {
            LinkageError::VerifyError {
                class_name,
                method_name,
                message,
            }
        }
        _ => err,
    }
}

/// `inner`, recording no verifier loads and publishing into no type-map store
/// of its own: the walk [`verify_deferred_class_bytecode`] runs is a rule
/// check, not the class's map walk (which records the loads, at define or at
/// link).
struct NoVerifierLoads<'a>(&'a dyn ClassHierarchy);

impl ClassHierarchy for NoVerifierLoads<'_> {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        self.0.is_subclass(child, parent)
    }

    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        self.0.is_direct_superclass(child, parent)
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        self.0.common_superclass(a, b)
    }

    fn is_interface(&self, name: &str) -> bool {
        self.0.is_interface(name)
    }

    fn is_resolvable(&self, name: &str) -> bool {
        self.0.is_resolvable(name)
    }

    fn type_map_store(&self) -> Option<&crate::type_maps::TypeMapStore> {
        self.0.type_map_store()
    }

    fn invokespecial_target(
        &self,
        current: &str,
        ref_class: &str,
    ) -> Option<super::vtype::SpecialTarget> {
        self.0.invokespecial_target(current, ref_class)
    }

    fn catch_type_is_throwable(&self, name: &str) -> Option<bool> {
        self.0.catch_type_is_throwable(name)
    }

    fn protected_receiver_rule(
        &self,
        current: &str,
        ref_class: &str,
        name: &str,
        descriptor: &str,
        is_method: bool,
    ) -> bool {
        self.0
            .protected_receiver_rule(current, ref_class, name, descriptor, is_method)
    }
}

/// The Pass 3 type-state verdict for a class a user-defined loader defines
/// (interpreter round i5 wave 101, lane L7; HotSpot verifies every such class,
/// `BytecodeVerificationRemote`). Runs where the define used to defer it
/// (`ClassManager`'s `defer_loader_sensitive_pass3`), through
/// [`UndecidedIsAssignable`]: HotSpot LOADS a class to answer an
/// assignability question, this verifier cannot load from inside a define, so
/// a question it cannot decide is answered "assignable" rather than refused --
/// the false rejections the deferral was introduced for. Off with
/// `CRATONVM_VERIFY_USER_LOADER_CLASSES=0`.
pub fn verify_user_loader_class_bytecode(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    if !loader_flags().verify_user_loader_classes {
        return Ok(());
    }
    verify_class_bytecode_with_strictness(class, &UndecidedIsAssignable(hierarchy, false), false)
}

/// [`verify_user_loader_class_bytecode`] that also records the verifier loads
/// in `hierarchy` (its `records_verifier_loads` cell), for the link's single
/// walk (`CRATONVM_USER_PASS3_RECORDS_LINK_LOADS`; interpreter round i6 wave
/// 119, lane L5, `i102-L5-proposal-one-walk-for-user-loader-pass3-and-type-maps`).
pub fn verify_user_loader_class_bytecode_recording(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    if !loader_flags().verify_user_loader_classes {
        return Ok(());
    }
    verify_class_bytecode_with_strictness(class, &UndecidedIsAssignable(hierarchy, true), false)
}

/// `inner`, except that `is_subclass` / `is_direct_superclass` answer `true`
/// when either name is not loaded yet (`is_resolvable` false), and no verifier
/// load is recorded (the deferred define path records them itself). Type maps
/// go to `inner`'s store (first writer wins; the walk's rows do not depend on
/// the assignability verdicts).
/// The `bool` (wave 119, lane L5): forward the verifier-load recording to
/// `inner` (the link's single walk); `false` records none, as before.
struct UndecidedIsAssignable<'a>(&'a dyn ClassHierarchy, bool);

impl UndecidedIsAssignable<'_> {
    fn undecided(&self, a: &str, b: &str) -> bool {
        let element = |n: &str| -> String {
            n.trim_start_matches('[')
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(n)
                .to_string()
        };
        !self.0.is_resolvable(&element(a)) || !self.0.is_resolvable(&element(b))
    }
}

impl ClassHierarchy for UndecidedIsAssignable<'_> {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        self.0.is_subclass(child, parent) || self.undecided(child, parent)
    }

    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        self.0.is_direct_superclass(child, parent) || self.undecided(child, parent)
    }

    fn records_verifier_loads(&self) -> bool {
        self.1 && self.0.records_verifier_loads()
    }

    fn note_verifier_load(&self, name: &str) {
        if self.1 {
            self.0.note_verifier_load(name)
        }
    }

    fn note_verifier_load_unless_interface(&self, target: &str, source: &str) {
        if self.1 {
            self.0.note_verifier_load_unless_interface(target, source)
        }
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        self.0.common_superclass(a, b)
    }

    fn is_interface(&self, name: &str) -> bool {
        self.0.is_interface(name)
    }

    fn is_resolvable(&self, name: &str) -> bool {
        self.0.is_resolvable(name)
    }

    fn type_map_store(&self) -> Option<&crate::type_maps::TypeMapStore> {
        self.0.type_map_store()
    }

    fn invokespecial_target(
        &self,
        current: &str,
        ref_class: &str,
    ) -> Option<super::vtype::SpecialTarget> {
        self.0.invokespecial_target(current, ref_class)
    }

    fn catch_type_is_throwable(&self, name: &str) -> Option<bool> {
        self.0.catch_type_is_throwable(name)
    }

    fn protected_receiver_rule(
        &self,
        current: &str,
        ref_class: &str,
        name: &str,
        descriptor: &str,
        is_method: bool,
    ) -> bool {
        self.0
            .protected_receiver_rule(current, ref_class, name, descriptor, is_method)
    }
}

/// Per-method form of [`verify_class_structural_bytecode`]. Takes the class
/// name rather than the `Class` so `bytecode_verifier::verify_method` — which
/// only ever holds `(&str, &ClassFileMethod, &ConstantPool, &ClassFileVersion)`
/// — can run the same scan.
pub(crate) fn verify_method_structural(
    class_name: &str,
    method: &ClassFileMethod,
) -> Result<(), LinkageError> {
    verify_method_structural_shared(class_name, method, &mut None)
}

/// [`verify_method_structural`] that shares the decode with its caller. SBR
/// round 5, lane verify: the structural scan, the type-state walk and the
/// class's subroutine pre-scan each fetched the method's decode from the
/// process cache (hash of the code bytes, a shard lock, a byte-for-byte
/// compare) -- three times per method. `decode` is the decode the caller
/// already holds (`Some`) or `None`; on success it holds the one this scan
/// used, for the caller's next step. The decode is still made at the same
/// point of the scan as before, so which error a method with several faults
/// reports is unchanged.
pub(crate) fn verify_method_structural_shared(
    class_name: &str,
    method: &ClassFileMethod,
    decode: &mut Option<DecodedInstructions>,
) -> Result<(), LinkageError> {
    let code_attr = match method.code() {
        Some(c) => c,
        None => return Ok(()),
    };
    let bytecode = &code_attr.code;
    let code_len = bytecode.len();

    // Empty bytecode in a non-abstract non-native method is malformed
    // (JVMS §4.9.1: "code_length ≥ 1").
    if code_len == 0 {
        return Err(LinkageError::VerifyError {
            class_name: class_name.to_string(),
            method_name: method.name.to_string(),
            message: "Code attribute has empty bytecode array".to_string(),
        });
    }

    // JVMS §4.9.1 / §4.10.1.6: `max_locals` must be large enough to hold the
    // method's own arguments — `this` (instance methods) plus every parameter,
    // with `long`/`double` counted as two slots.
    //
    // SECURITY: the interpreter allocates exactly `max_locals` slots. An
    // under-declared `max_locals` used to be invisible, because
    // `VerificationFrame::initial_frame` sizes its `locals` vector from the
    // descriptor and only *pads* up to `max_locals` — so the verifier modelled
    // MORE slots than the runtime frame has and every access in the overhang
    // verified clean while indexing past the end of the real frame.
    let argument_slots: usize =
        descriptor_param_slots(&method.descriptor) + usize::from(!method.is_static());
    if argument_slots > code_attr.max_locals as usize {
        return Err(LinkageError::VerifyError {
            class_name: class_name.to_string(),
            method_name: method.name.to_string(),
            message: format!(
                "max_locals is {} but the method's arguments occupy {argument_slots} local \
                 slots (descriptor {}); JVMS §4.9.1 requires max_locals to cover them",
                code_attr.max_locals, method.descriptor
            ),
        });
    }

    // Decode and validate all branch boundaries through the same decoder and
    // validation the compilers use (`verified_code` runs the same code), but
    // uncached: see `decode_instructions`. (The decode is handed back to the
    // caller on success, below.)
    let verified = match decode.take() {
        Some(v) => v,
        None => decode_instructions(bytecode).map_err(|e| LinkageError::VerifyError {
            class_name: class_name.to_string(),
            method_name: method.name.to_string(),
            message: format!("failed to build verified code: {e}"),
        })?,
    };
    // SBR round 5, lane warmup: this scan runs for EVERY method of EVERY loaded
    // class, so it is a startup cost. It used to clone every decoded
    // instruction into a second vector, hash every pc into two SipHash maps and
    // a set, and insert a per-pc `Vec` into a third map -- only the legacy
    // `jsr`/`ret` coverage analysis reads those maps. The instruction starts are
    // now a dense bitmap, and the maps are built only for a method that has a
    // `ret` (below). Same checks, same order, same errors.
    let verified_insns = verified.instructions();
    let instruction_starts = PcSet::from_starts(code_len, verified_insns.iter().map(|d| d.pc));

    // Validate exception handler ranges (JVMS §4.9.1).
    let mut jsr_sites = Vec::new();
    let mut ret_sites = Vec::new();

    for verified_insn in verified_insns {
        let decoded_insn = DecodedRef {
            pc: verified_insn.pc as usize,
            next_pc: verified_insn.next_pc as usize,
            insn: &verified_insn.instruction,
        };
        let pc = decoded_insn.pc;
        let next_pc = decoded_insn.next_pc;
        // Only the FIRST target is ever read (by the `jsr` arm below); the
        // scan used to collect every target of every instruction into a `Vec`.
        let mut first_target: Option<usize> = None;

        for target in small_branch_targets(decoded_insn.insn, pc).map_err(|message| {
            LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!("branch target at offset {pc} overflowed: {message}"),
            }
        })? {
            if target < 0 || target >= code_len as i64 {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "branch target {target} at offset {pc} is out of range \
                         (code length = {code_len})"
                    ),
                });
            }
            let target = target as usize;
            if !instruction_starts.contains(&target) {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "branch target {target} at offset {pc} does not land on an \
                         instruction boundary"
                    ),
                });
            }
            if first_target.is_none() {
                first_target = Some(target);
            }
        }

        // JVMS §4.9.1 static constraint: every local-variable operand must be
        // a slot the frame actually has. This is checked structurally — i.e.
        // for EVERY instruction in the method, reachable or not — because the
        // type-state walk only sees the instructions it reaches, and the
        // interpreter's unchecked local accessors index `Frame::locals` with
        // the raw operand.
        if let Some((index, width)) = local_operand(decoded_insn.insn) {
            let needed = u32::from(index) + u32::from(width);
            if needed > u32::from(code_attr.max_locals) {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "local variable index {index} at offset {pc} addresses {width} slot(s) \
                         but max_locals is {} (JVMS §4.9.1)",
                        code_attr.max_locals
                    ),
                });
            }
        }

        match decoded_insn.insn {
            Instruction::Jsr(_) | Instruction::JsrW(_) => {
                let Some(target) = first_target else {
                    return Err(LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("jsr at offset {pc} has no valid subroutine target"),
                    });
                };
                if next_pc >= code_len || !instruction_starts.contains(&next_pc) {
                    return Err(LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!(
                            "jsr return address {next_pc} at offset {pc} does not land on an \
                             instruction boundary"
                        ),
                    });
                }
                jsr_sites.push(JsrSite { target });
            }
            Instruction::Ret(index) => {
                if *index >= code_attr.max_locals {
                    return Err(LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!(
                            "ret at offset {pc} references local {index}, but max_locals is {}",
                            code_attr.max_locals
                        ),
                    });
                }
                ret_sites.push((pc, *index));
            }
            // JVMS §4.9.1: `wide` is a prefix; the decoder folds it into the
            // widened instruction, so a standalone `Wide` here means the byte
            // stream carried a `wide` with no valid successor opcode.
            Instruction::Wide => {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "stray `wide` prefix at offset {pc}: not followed by a widenable \
                         instruction (JVMS §4.9.1)"
                    ),
                });
            }
            _ => {}
        }
    }

    // The subroutine-coverage refusal, held until the handler checks ran.
    let mut coverage_error: Option<LinkageError> = None;
    if !ret_sites.is_empty() {
        // Legacy `jsr`/`ret` methods only: the coverage analysis wants the
        // owned instruction list and the by-pc maps the hot path no longer
        // builds. The scan above already proved every branch target in range,
        // so recomputing them cannot fail.
        let decoded: Vec<StructuralInstruction> = verified_insns
            .iter()
            .map(|d| StructuralInstruction {
                pc: d.pc as usize,
                next_pc: d.next_pc as usize,
                insn: d.instruction.clone(),
            })
            .collect();
        let instruction_by_pc: HashMap<_, _> = decoded
            .iter()
            .enumerate()
            .map(|(index, instruction)| (instruction.pc, index))
            .collect();
        let branch_targets_by_pc: HashMap<usize, Vec<usize>> = decoded
            .iter()
            .map(|d| {
                let targets = instruction_branch_targets(&d.insn, d.pc)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t as usize)
                    .collect();
                (d.pc, targets)
            })
            .collect();
        let covered_rets = covered_ret_sites(
            &decoded,
            &instruction_by_pc,
            &branch_targets_by_pc,
            &jsr_sites,
            code_len,
        );
        for (ret_pc, index) in ret_sites {
            if !covered_rets.contains(&(ret_pc, index)) {
                // Wave 105 hand-back (lane L7): reported AFTER the handler
                // table checks below, so a caller that lets the subroutine
                // walk overrule this coverage rule
                // (`verify_class_bytecode_inner`) still gets every other
                // structural refusal.
                coverage_error = Some(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "ret at offset {ret_pc} using local {index} is not covered by a \
                         reachable jsr/astore subroutine prologue"
                    ),
                });
                break;
            }
        }
    }

    for entry in &code_attr.exception_table {
        let start = entry.start_pc as usize;
        let end = entry.end_pc as usize;
        let handler = entry.handler_pc as usize;
        if start >= end {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!("exception handler range invalid: start_pc={start}, end_pc={end}"),
            });
        }
        if end > code_len {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!("exception handler end_pc={end} is past code length {code_len}"),
            });
        }
        if handler >= code_len {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!(
                    "exception handler handler_pc={handler} is past code length {code_len}"
                ),
            });
        }
        if !instruction_starts.contains(&start) {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!(
                    "exception handler start_pc={start} does not land on an instruction boundary"
                ),
            });
        }
        if end != code_len && !instruction_starts.contains(&end) {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!(
                    "exception handler end_pc={end} does not land on an instruction boundary"
                ),
            });
        }
        if !instruction_starts.contains(&handler) {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!(
                    "exception handler handler_pc={handler} does not land on an instruction boundary"
                ),
            });
        }
    }

    if let Some(e) = coverage_error {
        return Err(e);
    }
    *decode = Some(verified);
    Ok(())
}

/// HotSpot's refusal for the structural fault that made
/// [`verify_method_structural_shared`] refuse `method`, or `None` when this
/// replay does not find one it can name (the caller then keeps its own text).
///
/// Interpreter round i5 wave 103, lane L7
/// (`i102-L7-structural-scan-refuses-at-define`). The structural scan is this
/// VM's safety net, run in front of every type-state walk; HotSpot has no such
/// pass, its verifier meets the same faults in its own order and words:
///
/// * the split verifier (major >= 50; a major-50 class failing both verifiers
///   reports the split one's message): `generate_code_data` first ("Bad
///   instruction", `bad_code`), then `verify_exception_handler_table`
///   (`ClassFormatError` "Illegal exception table start_pc|end_pc|handler_pc N
///   in method '<external name>'", `class_format_error`), then the walk in
///   instruction order: a jump whose target has no frame because it is not an
///   instruction ("Expecting a stackmap frame at branch target N",
///   `missing_stackmap` at the jump), a local index past `max_locals`
///   (`get_local` / `set_local` / `set_local_2`: "Local variable table
///   overflow"; `get_local_2`: "get long/double overflows locals"; Reason
///   "Local index N is invalid"). The walk's type errors in earlier
///   instructions would come first on HotSpot; the scan runs before the walk.
/// * libverify (major < 50, `check_code.c`): `verify_opcode_operands` per
///   instruction ("Illegal target of jump or branch", "Illegal local variable
///   number" for every load / store / `iinc` / `ret` form), then `initialize_exception_table`
///   ("Illegal exception table range", "Illegal exception table handler").
///
/// Only an error path pays for it.
/// HotSpot's text for an invoke whose reserved operand bytes are not zero,
/// at `pc` of `code` (interpreter round i6 wave 116, lane L7): the decoder
/// (`Instruction::decode`) refuses such an instruction, and its "Bad
/// instruction" is not HotSpot's word for it. The split verifier's
/// `verify_invoke_instructions` says "Fourth operand byte of invokeinterface
/// must be zero" / "Third and fourth operand bytes of invokedynamic must be
/// zero"; libverify (`old`) says the former for `invokeinterface` and is not
/// answered here for `invokedynamic` (below major 51 it is not an opcode of
/// the class's version). `None` for any other decode failure.
fn reserved_operand_refusal(code: &[u8], pc: usize, old: bool) -> Option<&'static str> {
    let byte = |at: usize| code.get(pc + at).copied();
    match code.get(pc).copied()? {
        0xb9 if byte(4).is_some_and(|b| b != 0) => {
            Some("Fourth operand byte of invokeinterface must be zero")
        }
        0xba if !old && byte(4).is_some() && (byte(3) != Some(0) || byte(4) != Some(0)) => {
            Some("Third and fourth operand bytes of invokedynamic must be zero")
        }
        _ => None,
    }
}

pub(crate) fn hotspot_structural_refusal(
    class_name: &str,
    method: &ClassFileMethod,
    major: u16,
) -> Option<LinkageError> {
    use super::verify_hotspot_text::{render, HsMethod, Reason};
    let code_attr = method.code()?;
    let code: &[u8] = &code_attr.code;
    let code_len = code.len();
    if code_len == 0 {
        return None;
    }
    let max_locals = u32::from(code_attr.max_locals);
    let old = super::verify_old_text::uses_old_texts(major);
    let verify_error = |message: String| LinkageError::VerifyError {
        class_name: class_name.to_string(),
        method_name: method.name.to_string(),
        message,
    };
    let old_error = |reason: &str| {
        verify_error(super::verify_old_text::message(
            class_name,
            &method.name,
            &method.descriptor,
            reason,
        ))
    };
    let hs_method = HsMethod {
        class_name,
        method_name: &method.name,
        descriptor: &method.descriptor,
        code,
        exception_table: &code_attr.exception_table,
        stack_map_raw: super::bytecode_verifier::find_stack_map_table(&code_attr.attributes),
    };
    let split_error =
        |header: &str, bci: usize, reason: Reason| verify_error(render(&hs_method, header, bci, &reason, None, None));

    // Instruction starts (`generate_code_data` / libverify's first loop).
    let mut starts = vec![false; code_len];
    let mut insns: Vec<(usize, Instruction)> = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        match Instruction::decode(code, pc) {
            Ok((Instruction::Wide, _)) => {
                // A `wide` with no widenable successor.
                return (!old).then(|| split_error("Bad instruction", pc, Reason::BadCode));
            }
            Ok((insn, next_pc)) if next_pc > pc && next_pc <= code_len => {
                starts[pc] = true;
                insns.push((pc, insn));
                pc = next_pc;
            }
            // Interpreter round i6 wave 116, lane L7: the decoder refuses a
            // nonzero reserved operand byte, which HotSpot's opcode-length
            // pass accepts and its invoke check then words itself
            // (`verify_invoke_instructions`; libverify's
            // `verify_opcode_operands` for `invokeinterface`).
            _ if reserved_operand_refusal(code, pc, old).is_some() => {
                let header = reserved_operand_refusal(code, pc, old)?;
                return Some(if old {
                    old_error(header)
                } else {
                    split_error(header, pc, Reason::BadCode)
                });
            }
            _ => return (!old).then(|| split_error("Bad instruction", pc, Reason::BadCode)),
        }
    }
    let legal_target = |t: i64| t >= 0 && (t as usize) < code_len && starts[t as usize];

    if old {
        for (pc, insn) in &insns {
            let pc = *pc;
            // libverify's switch case (interpreter round i5 wave 109, lane
            // L7): the key order first, then the default, then each case
            // ("Illegal branch in tableswitch" for both opcodes).
            if let Instruction::Lookupswitch(ls) = insn {
                if ls.pairs.windows(2).any(|w| w[0].0 >= w[1].0) {
                    return Some(old_error("Unsorted lookup switch"));
                }
            }
            let targets = instruction_branch_targets(insn, pc).ok()?;
            if targets.iter().any(|&t| !legal_target(t)) {
                let at = |off: i32| pc as i64 + i64::from(off);
                let (default, cases): (i32, Vec<i32>) = match insn {
                    Instruction::Tableswitch(ts) => (ts.default, ts.offsets.clone()),
                    Instruction::Lookupswitch(ls) => {
                        (ls.default, ls.pairs.iter().map(|&(_, off)| off).collect())
                    }
                    _ => return Some(old_error("Illegal target of jump or branch")),
                };
                if !legal_target(at(default)) {
                    return Some(old_error("Illegal default target in switch"));
                }
                if cases.iter().any(|&off| !legal_target(at(off))) {
                    return Some(old_error("Illegal branch in tableswitch"));
                }
                return None;
            }
            let (index, width) = match insn {
                Instruction::Ret(i) => (u32::from(*i), 1),
                other => match local_operand(other) {
                    Some((i, w)) => (u32::from(i), u32::from(w)),
                    None => continue,
                },
            };
            if index + width > max_locals {
                // `verify_opcode_operands` checks every form against
                // `max_locals`: the short forms (`iload_3`, `lstore_1`) are
                // rewritten to the explicit opcode with their implied index
                // and take the same `check_local_variable[2]` path
                // (interpreter round i5 wave 109, lane L7: the wave-103 scan
                // let a short form fall back to this VM's own text).
                return Some(old_error("Illegal local variable number"));
            }
        }
        for e in code_attr.exception_table.iter() {
            let (start, end, handler) =
                (usize::from(e.start_pc), usize::from(e.end_pc), usize::from(e.handler_pc));
            let range_ok = start < end
                && end <= code_len
                && starts[start]
                && (end == code_len || starts[end]);
            if !range_ok {
                return Some(old_error("Illegal exception table range"));
            }
            if !(handler < code_len && starts[handler]) {
                return Some(old_error("Illegal exception table handler"));
            }
        }
        return None;
    }

    // Split verifier: the handler table, before the walk.
    for e in code_attr.exception_table.iter() {
        let (start, end, handler) =
            (usize::from(e.start_pc), usize::from(e.end_pc), usize::from(e.handler_pc));
        let refusal = if start >= code_len || !starts[start] {
            Some(format!("Illegal exception table start_pc {start}"))
        } else if end != code_len && (end > code_len || !starts[end]) {
            Some(format!("Illegal exception table end_pc {end}"))
        } else if handler >= code_len || !starts[handler] {
            Some(format!("Illegal exception table handler_pc {handler}"))
        } else {
            None
        };
        if let Some(text) = refusal {
            return Some(LinkageError::ClassFormatError {
                class_name: class_name.to_string(),
                message: format!(
                    "{text} in method '{}'",
                    external_method_name(class_name, &method.name, &method.descriptor)
                ),
            });
        }
    }
    // The walk, in instruction order.
    for (pc, insn) in &insns {
        let pc = *pc;
        match insn {
            // The subroutine opcodes have their own refusals (not replayed).
            Instruction::Jsr(_) | Instruction::JsrW(_) | Instruction::Ret(_) => return None,
            _ => {}
        }
        // `verify_switch` checks the key order before any target
        // (interpreter round i5 wave 109, lane L7)...
        if let Instruction::Lookupswitch(ls) = insn {
            if ls.pairs.windows(2).any(|w| w[0].0 >= w[1].0) {
                return Some(split_error("Bad lookupswitch instruction", pc, Reason::BadCode));
            }
        }
        // ...then the default, then each case, as
        // `instruction_branch_targets` lists them.
        let targets = instruction_branch_targets(insn, pc).ok()?;
        if let Some(&bad) = targets.iter().find(|&&t| !legal_target(t)) {
            return Some(split_error(
                &format!("Expecting a stackmap frame at branch target {bad}"),
                pc,
                Reason::MissingStackmap,
            ));
        }
        let header = match insn {
            Instruction::Iload(i)
            | Instruction::Fload(i)
            | Instruction::Aload(i)
            | Instruction::Istore(i)
            | Instruction::Fstore(i)
            | Instruction::Astore(i)
            | Instruction::Iinc { index: i, .. } => {
                (u32::from(*i) >= max_locals).then_some((*i, "Local variable table overflow"))
            }
            Instruction::Lload(i) | Instruction::Dload(i) => (u32::from(*i) + 1 >= max_locals)
                .then_some((*i, "get long/double overflows locals")),
            Instruction::Lstore(i) | Instruction::Dstore(i) => {
                (u32::from(*i) + 1 >= max_locals).then_some((*i, "Local variable table overflow"))
            }
            _ => None,
        };
        if let Some((index, header)) = header {
            return Some(split_error(header, pc, Reason::BadLocalIndex(index)));
        }
    }
    None
}

/// The split verifier's first refusal in a method WITHOUT a `StackMapTable`,
/// as far as it can be named without a type walk: `(bci, refusal, before)`,
/// `before` telling whether HotSpot meets it before the instruction's own
/// type checks. `None` when the method has a table, or the scan finds nothing.
///
/// `ClassVerifier::verify_method`, per instruction in code order:
/// `verify_stackmap_table` ("Expecting a stack map frame", `bad_code`, after an
/// unconditional transfer), the handler check of a store (before the store),
/// the instruction ("Bad instruction: %02x" for `jsr` / `jsr_w` / `ret`, which
/// the split verifier does not know; "Expecting a stackmap frame at branch
/// target N" from `check_jump_target`), then the handler check of any other
/// covered instruction ("Expecting a stackmap frame at branch target <handler>",
/// `missing_stackmap` at the covered bci).
fn split_frameless_scan(
    class_name: &str,
    method: &ClassFileMethod,
) -> Option<(usize, LinkageError, bool)> {
    use super::verify_hotspot_text::{render, HsMethod, Reason};
    let code_attr = method.code()?;
    if super::bytecode_verifier::find_stack_map_table(&code_attr.attributes).is_some() {
        return None;
    }
    let code: &[u8] = &code_attr.code;
    let hs_method = HsMethod {
        class_name,
        method_name: &method.name,
        descriptor: &method.descriptor,
        code,
        exception_table: &code_attr.exception_table,
        stack_map_raw: None,
    };
    let error = |header: &str, bci: usize, reason: Reason| LinkageError::VerifyError {
        class_name: class_name.to_string(),
        method_name: method.name.to_string(),
        message: render(&hs_method, header, bci, &reason, None, None),
    };
    let covering = |pc: usize| {
        code_attr
            .exception_table
            .iter()
            .find(|e| pc >= usize::from(e.start_pc) && pc < usize::from(e.end_pc))
            .map(|e| e.handler_pc)
    };
    let mut no_control_flow = false;
    let mut pc = 0usize;
    while pc < code.len() {
        let (insn, next_pc) = Instruction::decode(code, pc).ok()?;
        if next_pc <= pc {
            return None;
        }
        if no_control_flow {
            return Some((pc, error("Expecting a stack map frame", pc, Reason::BadCode), true));
        }
        let raw = if code[pc] == 0xc4 { *code.get(pc + 1)? } else { code[pc] };
        let is_store = (0x36..=0x4e).contains(&raw);
        let handler = covering(pc);
        let missing = |target: u16| {
            error(
                &format!("Expecting a stackmap frame at branch target {target}"),
                pc,
                Reason::MissingStackmap,
            )
        };
        if is_store {
            if let Some(h) = handler {
                return Some((pc, missing(h), true));
            }
        }
        match insn {
            Instruction::Jsr(_) | Instruction::JsrW(_) | Instruction::Ret(_) => {
                return Some((
                    pc,
                    error(&format!("Bad instruction: {raw:02x}"), pc, Reason::BadCode),
                    true,
                ));
            }
            _ => {}
        }
        let targets = instruction_branch_targets(&insn, pc).ok()?;
        if let Some(&first) = targets.first() {
            let target = u16::try_from(first).ok()?;
            return Some((pc, missing(target), false));
        }
        if !is_store {
            if let Some(h) = handler {
                return Some((pc, missing(h), false));
            }
        }
        // goto / goto_w / the switches leave above; returns and athrow.
        no_control_flow = matches!(raw, 0xac..=0xb1 | 0xbf);
        pc = next_pc;
    }
    None
}

/// HotSpot's message for a major-50 class whose `method` libverify refused at
/// `refusal_pc` (wave 104, lane L7, `i103-L7-jsr-methods-are-not-type-checked`):
/// the split verifier ran first, refused, failed over to libverify, and
/// `Verifier::verify` reports the SPLIT verifier's message when both refuse.
///
/// Answered only when it is certain: `method` has no `StackMapTable`, the
/// split verifier's first refusal in it is one [`split_frameless_scan`] names
/// and lies before libverify's (the straight-line prefix both walk is then
/// well-typed), and no other method of the class would be refused by the split
/// verifier (HotSpot walks the methods in its own sorted order, so which one
/// it reports would not be known). `None` keeps the caller's text.
pub(crate) fn split_refusal_of_frameless_method(
    class: &Class,
    method: &ClassFileMethod,
    refusal_pc: usize,
) -> Option<LinkageError> {
    let class_name: &str = &class.name;
    let (bci, error, before) = split_frameless_scan(class_name, method)?;
    if bci > refusal_pc || (bci == refusal_pc && !before) {
        return None;
    }
    for other in class.methods.iter() {
        if std::ptr::eq(other, method) || other.is_abstract() || other.is_native() {
            continue;
        }
        if split_frameless_scan(class_name, other).is_some() {
            return None;
        }
    }
    Some(error)
}

/// HotSpot's `Method::print_external_name`: `void p.C.m(int, java.lang.String)`.
pub(crate) fn external_method_name(class_name: &str, name: &str, descriptor: &str) -> String {
    fn external_type(piece: &str) -> String {
        let dims = piece.bytes().take_while(|&b| b == b'[').count();
        let base = &piece[dims..];
        let mut out = match base.as_bytes().first() {
            Some(b'B') => "byte".to_string(),
            Some(b'C') => "char".to_string(),
            Some(b'D') => "double".to_string(),
            Some(b'F') => "float".to_string(),
            Some(b'I') => "int".to_string(),
            Some(b'J') => "long".to_string(),
            Some(b'S') => "short".to_string(),
            Some(b'Z') => "boolean".to_string(),
            Some(b'V') => "void".to_string(),
            Some(b'L') => base
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(base)
                .replace('/', "."),
            _ => base.to_string(),
        };
        for _ in 0..dims {
            out.push_str("[]");
        }
        out
    }
    let (params, ret) = match descriptor.strip_prefix('(').and_then(|d| d.split_once(')')) {
        Some(parts) => parts,
        None => ("", descriptor),
    };
    let mut pieces: Vec<String> = Vec::new();
    let bytes = params.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            match params[i..].find(';') {
                Some(semi) => i += semi + 1,
                None => i = bytes.len(),
            }
        } else {
            i += 1;
        }
        pieces.push(external_type(params.get(start..i).unwrap_or("")));
    }
    format!(
        "{} {}.{}({})",
        external_type(ret),
        class_name.replace('/', "."),
        name,
        pieces.join(", ")
    )
}

/// The local-variable slot an instruction addresses, and how many slots it
/// occupies (2 for the `long`/`double` forms, 1 otherwise).
///
/// Returns `None` for instructions that do not name a local slot. `ret` is
/// handled separately (it has its own `max_locals` check plus the subroutine
/// coverage analysis), and `Wide` is rejected outright as a stray prefix.
fn local_operand(insn: &Instruction) -> Option<(u16, u16)> {
    match insn {
        Instruction::Iload(i)
        | Instruction::Fload(i)
        | Instruction::Aload(i)
        | Instruction::Istore(i)
        | Instruction::Fstore(i)
        | Instruction::Astore(i) => Some((*i, 1)),
        Instruction::Iinc { index, .. } => Some((*index, 1)),
        Instruction::Lload(i)
        | Instruction::Dload(i)
        | Instruction::Lstore(i)
        | Instruction::Dstore(i) => Some((*i, 2)),
        _ => None,
    }
}

fn covered_ret_sites(
    decoded: &[StructuralInstruction],
    instruction_by_pc: &HashMap<usize, usize>,
    branch_targets_by_pc: &HashMap<usize, Vec<usize>>,
    jsr_sites: &[JsrSite],
    code_len: usize,
) -> HashSet<(usize, u16)> {
    let mut saved_at: HashMap<usize, HashSet<u16>> = HashMap::new();
    let mut worklist = Vec::new();
    for site in jsr_sites {
        let entry = saved_at.entry(site.target).or_default();
        if entry.is_empty() {
            worklist.push(site.target);
        }
    }

    let mut covered = HashSet::new();
    while let Some(pc) = worklist.pop() {
        let Some(&idx) = instruction_by_pc.get(&pc) else {
            continue;
        };
        let decoded_insn = &decoded[idx];
        let saved = saved_at.get(&pc).cloned().unwrap_or_default();

        if let Instruction::Ret(index) = &decoded_insn.insn {
            if saved.contains(index) {
                covered.insert((pc, *index));
            }
            continue;
        }

        let mut out_saved = saved;
        if let Instruction::Astore(index) = &decoded_insn.insn {
            out_saved.insert(*index);
        }

        let branch_targets = branch_targets_by_pc
            .get(&pc)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for succ in structural_successors(decoded_insn, branch_targets, code_len) {
            match saved_at.entry(succ) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(out_saved.clone());
                    worklist.push(succ);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let before = entry.get().len();
                    entry.get_mut().extend(out_saved.iter().copied());
                    if entry.get().len() != before {
                        worklist.push(succ);
                    }
                }
            }
        }
    }
    covered
}

fn structural_successors(
    decoded_insn: &StructuralInstruction,
    branch_targets: &[usize],
    code_len: usize,
) -> Vec<usize> {
    let mut successors = Vec::new();
    match &decoded_insn.insn {
        Instruction::Goto(_)
        | Instruction::GotoW(_)
        | Instruction::Jsr(_)
        | Instruction::JsrW(_) => {
            successors.extend_from_slice(branch_targets);
        }
        Instruction::Ifeq(_)
        | Instruction::Ifne(_)
        | Instruction::Iflt(_)
        | Instruction::Ifge(_)
        | Instruction::Ifgt(_)
        | Instruction::Ifle(_)
        | Instruction::IfIcmpeq(_)
        | Instruction::IfIcmpne(_)
        | Instruction::IfIcmplt(_)
        | Instruction::IfIcmpge(_)
        | Instruction::IfIcmpgt(_)
        | Instruction::IfIcmple(_)
        | Instruction::IfAcmpeq(_)
        | Instruction::IfAcmpne(_)
        | Instruction::Ifnull(_)
        | Instruction::Ifnonnull(_) => {
            successors.extend_from_slice(branch_targets);
            if decoded_insn.next_pc < code_len {
                successors.push(decoded_insn.next_pc);
            }
        }
        Instruction::Tableswitch(_) | Instruction::Lookupswitch(_) => {
            successors.extend_from_slice(branch_targets);
        }
        Instruction::Ret(_)
        | Instruction::Ireturn
        | Instruction::Lreturn
        | Instruction::Freturn
        | Instruction::Dreturn
        | Instruction::Areturn
        | Instruction::Return
        | Instruction::Athrow => {}
        _ => {
            if decoded_insn.next_pc < code_len {
                successors.push(decoded_insn.next_pc);
            }
        }
    }
    successors
}

/// Compute the absolute branch targets reachable from `insn` at `pc`.
///
/// Mirrors the `branch_targets` field that `verify_instruction` returns
/// in its `InsnVerifyResult`, but is callable without invoking the full
/// type-state machinery. Returns absolute u16 offsets into the bytecode
/// array; the caller checks them against `code.len()`. Includes the
/// fall-through targets of conditional branches, the default and case
/// arms of `tableswitch`/`lookupswitch`, and the targets of
/// `goto`/`goto_w`/`jsr`/`jsr_w`. `ret` is intentionally excluded — its
/// target is data-flow-dependent and cannot be validated structurally.
/// The branch targets of one instruction, without a heap allocation for the
/// common 0- or 1-target case ([`instruction_branch_targets`] allocates a `Vec`
/// per branching instruction).
enum SmallTargets {
    None,
    One(i64),
    Many(Vec<i64>),
}

enum SmallTargetsIter {
    None,
    One(Option<i64>),
    Many(std::vec::IntoIter<i64>),
}

impl Iterator for SmallTargetsIter {
    type Item = i64;

    #[inline]
    fn next(&mut self) -> Option<i64> {
        match self {
            SmallTargetsIter::None => None,
            SmallTargetsIter::One(t) => t.take(),
            SmallTargetsIter::Many(it) => it.next(),
        }
    }
}

impl IntoIterator for SmallTargets {
    type Item = i64;
    type IntoIter = SmallTargetsIter;

    #[inline]
    fn into_iter(self) -> SmallTargetsIter {
        match self {
            SmallTargets::None => SmallTargetsIter::None,
            SmallTargets::One(t) => SmallTargetsIter::One(Some(t)),
            SmallTargets::Many(v) => SmallTargetsIter::Many(v.into_iter()),
        }
    }
}

/// Same targets, in the same order, as [`instruction_branch_targets`].
fn small_branch_targets(insn: &Instruction, pc: usize) -> Result<SmallTargets, &'static str> {
    let offset = match insn {
        Instruction::Ifeq(o)
        | Instruction::Ifne(o)
        | Instruction::Iflt(o)
        | Instruction::Ifge(o)
        | Instruction::Ifgt(o)
        | Instruction::Ifle(o)
        | Instruction::IfIcmpeq(o)
        | Instruction::IfIcmpne(o)
        | Instruction::IfIcmplt(o)
        | Instruction::IfIcmpge(o)
        | Instruction::IfIcmpgt(o)
        | Instruction::IfIcmple(o)
        | Instruction::IfAcmpeq(o)
        | Instruction::IfAcmpne(o)
        | Instruction::Ifnull(o)
        | Instruction::Ifnonnull(o)
        | Instruction::Goto(o)
        | Instruction::Jsr(o) => *o as i64,
        Instruction::GotoW(o) | Instruction::JsrW(o) => *o as i64,
        Instruction::Tableswitch(_) | Instruction::Lookupswitch(_) => {
            return instruction_branch_targets(insn, pc).map(SmallTargets::Many);
        }
        _ => return Ok(SmallTargets::None),
    };
    let pc_i64 = i64::try_from(pc).map_err(|_| "program counter does not fit in i64")?;
    pc_i64
        .checked_add(offset)
        .map(SmallTargets::One)
        .ok_or("program counter plus branch offset overflowed")
}

pub(crate) fn instruction_branch_targets(
    insn: &Instruction,
    pc: usize,
) -> Result<Vec<i64>, &'static str> {
    let pc_i64 = i64::try_from(pc).map_err(|_| "program counter does not fit in i64")?;
    let target = |off: i64| -> Result<i64, &'static str> {
        pc_i64
            .checked_add(off)
            .ok_or("program counter plus branch offset overflowed")
    };
    match insn {
        Instruction::Ifeq(o)
        | Instruction::Ifne(o)
        | Instruction::Iflt(o)
        | Instruction::Ifge(o)
        | Instruction::Ifgt(o)
        | Instruction::Ifle(o)
        | Instruction::IfIcmpeq(o)
        | Instruction::IfIcmpne(o)
        | Instruction::IfIcmplt(o)
        | Instruction::IfIcmpge(o)
        | Instruction::IfIcmpgt(o)
        | Instruction::IfIcmple(o)
        | Instruction::IfAcmpeq(o)
        | Instruction::IfAcmpne(o)
        | Instruction::Ifnull(o)
        | Instruction::Ifnonnull(o)
        | Instruction::Goto(o)
        | Instruction::Jsr(o) => Ok(vec![target(*o as i64)?]),
        Instruction::GotoW(o) | Instruction::JsrW(o) => Ok(vec![target(*o as i64)?]),
        Instruction::Tableswitch(ts) => {
            let mut v = Vec::with_capacity(ts.offsets.len() + 1);
            v.push(target(ts.default as i64)?);
            for off in &ts.offsets {
                v.push(target(*off as i64)?);
            }
            Ok(v)
        }
        Instruction::Lookupswitch(ls) => {
            let mut v = Vec::with_capacity(ls.pairs.len() + 1);
            v.push(target(ls.default as i64)?);
            for (_, off) in &ls.pairs {
                v.push(target(*off as i64)?);
            }
            Ok(v)
        }
        // `ret` jumps to a returnAddress stored in a local — the target
        // is dynamic, not encoded in the instruction. Not checked here.
        _ => Ok(Vec::new()),
    }
}

/// Resolve the owner class, method name, and descriptor of a `Methodref` /
/// `InterfaceMethodref` constant-pool entry referenced by an
/// `invokespecial`/`invoke*` index.
///
/// Returns `Some((owner_class_name, method_name, descriptor))` when the index
/// names a (interface-)method reference whose owner and `NameAndType` both
/// resolve. Used by [`check_new_init_owner_match`] to recover the
/// constructor's declaring class and arity for the JVMS §4.10.1.9
/// owner-match check.
fn resolve_invoked_owner_and_name(
    cp: &ConstantPool,
    index: u16,
) -> Option<(
    std::sync::Arc<str>,
    std::sync::Arc<str>,
    std::sync::Arc<str>,
)> {
    let (class_index, name_and_type_index) = match cp.get(index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner = cp.get_class_name_arc(class_index)?;
    let (name, desc) = cp.get_name_and_type(name_and_type_index)?;
    Some((
        owner,
        std::sync::Arc::from(name),
        std::sync::Arc::from(desc),
    ))
}

/// JVMS §4.10.1.9 owner-match check for `invokespecial <init>`.
///
/// When `insn` is an `invokespecial` of an `<init>` whose receiver (after the
/// constructor arguments are accounted for) is an `Uninitialized(offset)`
/// value, the type created by the `new` at `offset` must be the *same class*
/// as the constructor's declaring class. The plain type-state pass cannot
/// enforce this on its own because `VType::Uninitialized(offset)` carries only
/// the bytecode offset, not the class name — so we thread `new_site_classes`
/// (populated when each `new` instruction is decoded) and compare here.
///
/// This runs **before** `verify_instruction` consumes the frame, reading the
/// receiver non-destructively from `frame.stack`. The receiver sits just below
/// the constructor's argument slots, so we skip exactly the argument width
/// (category-2 args occupy two slots) to find it.
///
/// A mismatch (`new C; ... ; invokespecial D.<init>` with `C != D`) is a
/// `VerifyError`; the `new`-site type and the constructor owner disagree, which
/// would let a constructor run against the wrong uninitialized object shape.
/// `UninitializedThis` receivers are handled by `verify_instruction` itself
/// (the current-class / superclass check) and are ignored here.
fn check_new_init_owner_match(
    insn: &Instruction,
    frame: &VerificationFrame,
    cp: &ConstantPool,
    new_site_classes: &std::collections::HashMap<u16, std::sync::Arc<str>>,
    class_name: &str,
    method_name: &str,
) -> Result<(), LinkageError> {
    let index = match insn {
        Instruction::Invokespecial(index) => *index,
        _ => return Ok(()),
    };
    // (`resolve_invoked_owner_and_name`, borrowed: this runs for every
    // `invokespecial` of every walked method and used to copy the name and the
    // descriptor into fresh `Arc<str>`s before looking at either.)
    let (class_index, name_and_type_index) = match cp.get(index) {
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        })
        | Some(ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        _ => return Ok(()), // unresolved ref — verify_instruction reports it
    };
    let Some(owner) = cp.get_class_name_arc(class_index) else {
        return Ok(()); // unresolved ref — verify_instruction reports it
    };
    let Some((invoked_name, descriptor)) = cp.get_name_and_type(name_and_type_index) else {
        return Ok(()); // unresolved ref — verify_instruction reports it
    };
    if invoked_name != "<init>" {
        return Ok(());
    }

    // Count the operand-stack slots the constructor arguments occupy so we can
    // index past them to the receiver. Category-2 params take two slots.
    let arg_slots: usize = descriptor_param_slots(descriptor);

    // Receiver is the slot directly beneath the argument slots. If the stack
    // is too shallow, the type-state pass will report the underflow — bail.
    let depth = frame.stack.len();
    if depth < arg_slots + 1 {
        return Ok(());
    }
    let receiver = &frame.stack[depth - arg_slots - 1];

    if let VType::Uninitialized(offset) = receiver {
        // Look up the class created by the `new` at `offset`. If we never saw
        // the `new` (e.g. the uninitialized value arrived via a declared
        // StackMapTable frame rather than a decoded `new`), we can't compare —
        // skip rather than risk a false rejection.
        if let Some(new_class) = new_site_classes.get(offset) {
            if **new_class != *owner {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method_name.to_string(),
                    message: format!(
                        "invokespecial <init>: constructor owner {owner} does not match \
                         the type {new_class} created by `new` at offset {offset} \
                         (JVMS §4.10.1.9)"
                    ),
                });
            }
        }
    }
    Ok(())
}

/// True if `version` predates StackMapTable (Java 6 and earlier).
///
/// Used to gate which class files may legitimately contain `jsr`/`ret`.
/// Per JVMS §4.10.1 a class with major version ≥ 51 (Java 7+) MUST NOT
/// emit `jsr` or `ret`; the standard verifier rejects them on that path.
#[allow(dead_code)] // referenced by tests below
fn is_pre_java7(version: &ClassFileVersion) -> bool {
    version.major < ClassFileVersion::JAVA_7.major
}

/// Verify the structural integrity of a loaded class (Pass 2 only).
///
/// Useful when you only need structural checks without bytecode verification.
pub fn verify_class_structure(class: &Class, store: &ClassStore) -> Result<(), LinkageError> {
    verify_class_access_flags(class)?;
    verify_method_access_flags(class)?;
    verify_final_class_constraint(class, store)?;
    verify_final_method_constraint(class, store)?;
    // NOTE: `verify_abstract_method_implementation` (which also calls
    // `verify_interface_methods`) is currently disabled because its
    // simplified `find_method_recursive` model rejects legitimate JDK + 3rd-
    // party classes (Hashtable.size, BouncyCastleProvider, Spring boot loader,
    // ByteBuddy GetSystemPropertyAction, Kafka logIdent, …). The JVM spec
    // permits miranda methods, interface default methods inherited via the
    // class-hierarchy walk, and other corner cases that our walk misses.
    // Re-enable once the walk matches JVMS §5.4.3.3 method resolution.
    let _ = verify_abstract_method_implementation;
    // HotSpot does not reject every concrete class that leaves an inherited
    // abstract superclass method unimplemented; linkage may still proceed and
    // an AbstractMethodError is raised only if that method is actually invoked.
    // JAXB's concrete `JAXBContextImpl` is a real-world example: it inherits
    // the deprecated abstract `JAXBContext.createValidator()` and has no
    // override, but the class is valid on HotSpot. Keep this checker compiled
    // for focused tests, but do not enforce it during ordinary class loading.
    let _ = verify_inherited_abstract_methods_implemented;
    verify_code_attribute_presence(class)?;
    Ok(())
}

/// A non-abstract (concrete), non-interface class must provide a concrete
/// implementation for every abstract method it inherits from its superclass
/// chain (JVMS §5.4.3.3 / AbstractMethodError-class structural check).
///
/// Scope: this check is intentionally limited to abstract methods inherited
/// through the **superclass** chain. Interface (abstract/default) methods are
/// NOT modeled here — the broader interface walk in
/// `verify_abstract_method_implementation` / `verify_interface_methods` is
/// disabled because its simplified resolution rejects legitimate JDK and
/// third-party classes (miranda methods, inherited interface defaults, …).
/// Superclass-inherited abstract methods are unambiguous, so we can enforce
/// them safely.
///
/// "Implemented vs not": for each abstract method declared somewhere on the
/// superclass chain, we walk from the concrete class up to (but not including)
/// the declaring ancestor, looking for a method with the exact same
/// name+descriptor that is itself concrete (non-abstract). If such an override
/// exists anywhere in that span, the abstract method is considered implemented.
fn verify_inherited_abstract_methods_implemented(
    class: &Class,
    store: &ClassStore,
) -> Result<(), LinkageError> {
    // Only concrete (non-abstract, non-interface) classes must implement
    // inherited abstract methods. An abstract class (or interface) may leave
    // them unimplemented.
    if class.is_abstract() || class.is_interface() {
        return Ok(());
    }

    // Synthetic-stub classes carry no `.class` file: their method tables are
    // curated/empty and the contract is fulfilled entirely by native
    // registrations (see `Class::is_synthetic_stub`). Applying a *bytecode*-
    // level abstract-implementation check to them is categorically wrong — it
    // false-positives on inherited abstract methods that the VM satisfies via
    // native dispatch. This was the regression that broke ALL reflection:
    // `java/lang/reflect/Constructor` (a synthetic stub) inherits the
    // package-private abstract `Executable.getAnnotationBytes()[B`, which the
    // real JDK overrides but our stub's curated method table omits — so this
    // check rejected `Constructor`, and every `Class.getDeclaredMethods()`
    // call (which links `Constructor`) died with a verification error.
    if class.origin.is_compatibility_stub() {
        return Ok(());
    }

    // Walk the superclass chain, inspecting each ancestor's declared abstract
    // methods.
    let mut current_id = class.superclass;
    while let Some(ancestor_id) = current_id {
        let ancestor = match store.get(ancestor_id) {
            Some(c) => c,
            None => break,
        };

        for method in &ancestor.methods {
            if !method.is_abstract() || method.is_static() {
                continue;
            }
            // Constructors / class initializers are never abstract overrides.
            if method.name.starts_with('<') {
                continue;
            }

            // Look for a concrete override with the exact same name+descriptor
            // anywhere from the concrete class (inclusive) up to — but not
            // including — the declaring ancestor.
            let mut scan_id = Some(class.id);
            let mut implemented = false;
            while let Some(id) = scan_id {
                if id == ancestor_id {
                    break;
                }
                // The class under verification is NOT yet inserted into
                // `store` at this point (verify_class_structure runs before the
                // store insert — see class_manager.rs), so `store.get(class.id)`
                // returns None. Consult the `class` parameter directly for its
                // own id; otherwise the very first scan hop breaks and every
                // concrete class that overrides an inherited abstract method is
                // falsely rejected (Integer.intValue, StringBuilder.toString,
                // reflect.Method/Constructor.getAnnotationBytes — the regression
                // that broke all reflection).
                let scan_class = if id == class.id {
                    class
                } else {
                    match store.get(id) {
                        Some(c) => c,
                        None => break,
                    }
                };
                if let Some(found) = scan_class.find_method(&method.name, &method.descriptor) {
                    if !found.is_abstract() {
                        implemented = true;
                        break;
                    }
                }
                scan_id = scan_class.superclass;
            }

            if !implemented {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "concrete class must implement abstract method {}.{}{}",
                        ancestor.name, method.name, method.descriptor,
                    ),
                });
            }
        }

        current_id = ancestor.superclass;
    }

    Ok(())
}

/// Check that class access flags don't have conflicting combinations.
fn verify_class_access_flags(class: &Class) -> Result<(), LinkageError> {
    let flags = class.access_flags;

    // FINAL + ABSTRACT is invalid (you can't extend and also must extend)
    if flags.contains(ClassAccessFlags::FINAL) && flags.contains(ClassAccessFlags::ABSTRACT) {
        return Err(LinkageError::ClassFormatError {
            class_name: class.name.to_string(),
            message: "class cannot be both FINAL and ABSTRACT".to_string(),
        });
    }

    // INTERFACE requires ABSTRACT (JVM spec 4.1)
    if flags.contains(ClassAccessFlags::INTERFACE) && !flags.contains(ClassAccessFlags::ABSTRACT) {
        return Err(LinkageError::ClassFormatError {
            class_name: class.name.to_string(),
            message: "interface must be ABSTRACT".to_string(),
        });
    }

    // INTERFACE + FINAL is invalid
    if flags.contains(ClassAccessFlags::INTERFACE) && flags.contains(ClassAccessFlags::FINAL) {
        return Err(LinkageError::ClassFormatError {
            class_name: class.name.to_string(),
            message: "interface cannot be FINAL".to_string(),
        });
    }

    // ANNOTATION requires INTERFACE
    if flags.contains(ClassAccessFlags::ANNOTATION) && !flags.contains(ClassAccessFlags::INTERFACE)
    {
        return Err(LinkageError::ClassFormatError {
            class_name: class.name.to_string(),
            message: "ANNOTATION flag requires INTERFACE flag".to_string(),
        });
    }

    Ok(())
}

/// Check that method access flags don't have conflicting combinations.
fn verify_method_access_flags(class: &Class) -> Result<(), LinkageError> {
    for method in &class.methods {
        let flags = method.access_flags;

        // ABSTRACT methods cannot be PRIVATE, STATIC, FINAL, SYNCHRONIZED, NATIVE, or STRICT
        if flags.contains(MethodAccessFlags::ABSTRACT) {
            let invalid = MethodAccessFlags::PRIVATE
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::FINAL
                | MethodAccessFlags::SYNCHRONIZED
                | MethodAccessFlags::NATIVE
                | MethodAccessFlags::STRICT;

            if flags.intersects(invalid) {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!("abstract method cannot have flags: {:?}", flags & invalid),
                });
            }
        }

        // In an interface, methods must be PUBLIC (or PRIVATE in Java 9+)
        // We allow PRIVATE for Java 9+ compatibility
        if class.is_interface()
            && !method.name.starts_with('<')
            && !flags.contains(MethodAccessFlags::PUBLIC)
            && !flags.contains(MethodAccessFlags::PRIVATE)
        {
            return Err(LinkageError::VerifyError {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                message: "interface method must be PUBLIC or PRIVATE".to_string(),
            });
        }
    }

    Ok(())
}

/// Cannot extend a FINAL class.
fn verify_final_class_constraint(class: &Class, store: &ClassStore) -> Result<(), LinkageError> {
    if let Some(super_id) = class.superclass {
        match store.get(super_id) {
            Some(super_class) => {
                if super_class.is_final() {
                    return Err(LinkageError::VerifyError {
                        class_name: class.name.to_string(),
                        method_name: String::new(),
                        message: format!("cannot extend final class {}", super_class.name),
                    });
                }
            }
            None => {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: String::new(),
                    message: "superclass not found during verification".to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Cannot override a FINAL method from a superclass.
///
/// Mirrors HotSpot's `ClassFileParser::check_final_method_override`
/// (classFileParser.cpp, JDK 25):
/// * only the subclass's non-private, non-static, non-`<init>` methods can
///   override anything (JVMS 5.4.5: a private method never overrides; a
///   class may declare its own private `getClass()Ljava/lang/Class;` even
///   though `Object.getClass` is final -- HotSpot links it);
/// * the superclass chain is searched with a lookup that also finds private
///   and static declarations; a match that is final, non-static,
///   non-private AND accessible from the subclass
///   (`Reflection::verify_member_access`: public/protected always, package
///   private only from the same runtime package) is an illegal override;
/// * any other match does not end the search: it continues from the
///   matching method's holder's superclass, so a final method further up is
///   still found behind a non-final / private / static / inaccessible
///   same-signature declaration in between.
fn verify_final_method_constraint(class: &Class, store: &ClassStore) -> Result<(), LinkageError> {
    // Only check if we have a superclass
    let super_id = match class.superclass {
        Some(id) => id,
        None => return Ok(()),
    };

    for method in &class.methods {
        // HotSpot: skip private, static, and <init> methods (<clinit> is
        // static in every class file HotSpot accepts; skipped as before).
        if method.is_static()
            || method.access_flags.contains(MethodAccessFlags::PRIVATE)
            || &*method.name == "<init>"
            || &*method.name == "<clinit>"
        {
            continue;
        }

        // Walk the superclass chain like HotSpot: find the nearest
        // same-signature declaration; if it is not an accessible final
        // instance method, keep searching above its holder.
        let mut next = Some(super_id);
        while let Some(k_id) = next {
            let Some(k) = store.get(k_id) else {
                break;
            };
            next = k.superclass;
            let Some(super_method) = k.find_method(&method.name, &method.descriptor) else {
                continue;
            };
            if !super_method.access_flags.contains(MethodAccessFlags::FINAL)
                || super_method.is_static()
                || super_method
                    .access_flags
                    .contains(MethodAccessFlags::PRIVATE)
            {
                continue;
            }
            // SBR5 lane jfr: HotSpot's JFR schema extension gives every
            // concrete `jdk.jfr.Event` subclass its own `begin`/`end`/
            // `commit`/`isEnabled`/`shouldCommit` although `jdk.jfr.Event`
            // declares them final; the class parser does not apply the
            // final-override rule to those five.
            if &*k.name == "jdk/jfr/Event"
                && matches!(
                    &*method.name,
                    "begin" | "end" | "commit" | "isEnabled" | "shouldCommit"
                )
            {
                continue;
            }
            // JVMS 5.4.5: a package-private method is only inherited (and
            // therefore overridable) by classes in the same runtime
            // package. A same-signature method declared in a DIFFERENT
            // package is a new method, not an override, so the final
            // check must not fire. Live case: glassfish
            // ManagedScheduledThreadPoolExecutor.reject(Runnable) vs
            // j.u.c.ThreadPoolExecutor final package-private
            // reject(Runnable) -- HotSpot links it; our bogus VerifyError
            // failed every WildFly EE-concurrent default executor.
            let is_package_private = !super_method.access_flags.intersects(
                MethodAccessFlags::PUBLIC
                    | MethodAccessFlags::PROTECTED
                    | MethodAccessFlags::PRIVATE,
            );
            // JIT round 20 rev13 (correctness, unconditional): the RUNTIME
            // package (JVMS 5.3: package name AND defining loader), as
            // HotSpot's `verify_member_access` compares it. A same-named
            // package of another loader is another package: the final
            // method is not accessible there, so it is not overridden and
            // HotSpot links the class.
            //
            // Lane rev14: the loader test is `same_defining_loader_in` (the
            // built-in loaders are one outside a split boot package; a raw id
            // test let a mislabelled JDK-package class override a final
            // method), and the package is `class_package`, which cuts a
            // hidden class's `/0x…` suffix.
            if is_package_private
                && (crate::method_override::class_package(k)
                    != crate::method_override::class_package(class)
                    || !crate::method_override::same_defining_loader_in(
                        store,
                        k.loader_id,
                        class.loader_id,
                        crate::method_override::class_package(class),
                    ))
            {
                continue;
            }
            // HotSpot 25 throws java.lang.IncompatibleClassChangeError here
            // (host-measured, HotSpot 25.0.4, probe R20FinalOverrideMsg),
            // text "class %s overrides final method %s.%s%s": external
            // (dotted) names of the class and of the final method's holder,
            // then the raw name and descriptor. `linkage_throwable` renders
            // this variant's message verbatim.
            return Err(LinkageError::IncompatibleClassChangeError {
                message: format!(
                    "class {} overrides final method {}.{}{}",
                    class.name.replace('/', "."),
                    k.name.replace('/', "."),
                    method.name,
                    method.descriptor,
                ),
            });
        }
    }

    Ok(())
}

/// Non-abstract, non-interface classes must implement all inherited abstract methods.
fn verify_abstract_method_implementation(
    class: &Class,
    store: &ClassStore,
) -> Result<(), LinkageError> {
    // Only applies to concrete (non-abstract, non-interface) classes
    if class.is_abstract() || class.is_interface() {
        return Ok(());
    }

    // Collect abstract methods from superclass chain
    let mut current_id = class.superclass;
    while let Some(ancestor_id) = current_id {
        let ancestor = match store.get(ancestor_id) {
            Some(c) => c,
            None => break,
        };

        for method in &ancestor.methods {
            if !method.is_abstract() {
                continue;
            }
            // Skip private/static abstract (shouldn't exist, but be defensive)
            if method.is_static() {
                continue;
            }

            // Check if our class (or any of its ancestors closer than the declaring class)
            // provides a concrete implementation
            let has_impl = find_method_recursive(class.id, &method.name, &method.descriptor, store)
                .map(|(m, _)| !m.is_abstract())
                .unwrap_or(false);

            if !has_impl {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "concrete class must implement abstract method {}.{}{}",
                        ancestor.name, method.name, method.descriptor,
                    ),
                });
            }
        }

        current_id = ancestor.superclass;
    }

    // Check interface methods — concrete classes must implement all
    // abstract methods declared in directly implemented interfaces
    // and their super-interfaces.
    verify_interface_methods(class, store)?;

    Ok(())
}

/// Verify that a concrete class implements all abstract methods from its interfaces.
///
/// Walks the directly-implemented interfaces (and recursively their super-interfaces)
/// collecting abstract methods that the class must provide.
fn verify_interface_methods(class: &Class, store: &ClassStore) -> Result<(), LinkageError> {
    let mut iface_queue: Vec<super::ClassId> = class.interfaces.clone();
    let mut visited = std::collections::HashSet::new();

    while let Some(iface_id) = iface_queue.pop() {
        if !visited.insert(iface_id) {
            continue;
        }
        let iface = match store.get(iface_id) {
            Some(c) => c,
            None => continue,
        };

        for method in &iface.methods {
            if !method.is_abstract() {
                continue;
            }
            // Static and private interface methods don't need implementation
            if method.is_static() || method.access_flags.contains(MethodAccessFlags::PRIVATE) {
                continue;
            }
            // Skip <init> and <clinit>
            if method.name.starts_with('<') {
                continue;
            }

            let has_impl = find_method_recursive(class.id, &method.name, &method.descriptor, store)
                .map(|(m, _)| !m.is_abstract())
                .unwrap_or(false);

            if !has_impl {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "concrete class must implement interface method {}.{}{}",
                        iface.name, method.name, method.descriptor,
                    ),
                });
            }
        }

        // Recurse into super-interfaces
        iface_queue.extend_from_slice(&iface.interfaces);
    }

    Ok(())
}

/// Non-abstract, non-native methods must have a Code attribute.
/// Abstract and native methods must NOT have a Code attribute.
fn verify_code_attribute_presence(class: &Class) -> Result<(), LinkageError> {
    for method in &class.methods {
        let has_code = method.code().is_some();
        let is_abstract = method.is_abstract();
        let is_native = method.is_native();

        if !is_abstract && !is_native && !has_code {
            return Err(LinkageError::VerifyError {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                message: "non-abstract non-native method must have Code attribute".to_string(),
            });
        }

        if (is_abstract || is_native) && has_code {
            return Err(LinkageError::VerifyError {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                message: "abstract/native method must not have Code attribute".to_string(),
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class::{ClassId, ClassLoaderId, ClassState};

    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;

    fn empty_cp() -> ConstantPool {
        ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
    }

    fn make_class_in_store(
        store: &mut ClassStore,
        name: &str,
        superclass: Option<ClassId>,
        flags: ClassAccessFlags,
        methods: Vec<ClassFileMethod>,
    ) -> ClassId {
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_cp(),
            access_flags: flags,
            superclass,
            interfaces: vec![],
            fields: vec![],
            methods,
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: crate::class_origin::ClassOrigin::VmInternal,
            has_finalizer: false,
            signature: None,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        });
        id
    }

    fn make_method(
        name: &str,
        desc: &str,
        flags: MethodAccessFlags,
        has_code: bool,
    ) -> ClassFileMethod {
        let attributes = if has_code {
            vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))]
        } else {
            vec![]
        };

        ClassFileMethod {
            access_flags: flags,
            name: Arc::from(name),
            descriptor: Arc::from(desc),
            attributes,
        }
    }

    // --- Access flag validity ---

    #[test]
    fn final_and_abstract_class_rejected() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Bad",
            None,
            ClassAccessFlags::FINAL | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
            vec![],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_err());
    }

    #[test]
    fn interface_without_abstract_rejected() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Bad",
            None,
            ClassAccessFlags::INTERFACE, // missing ABSTRACT
            vec![],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_err());
    }

    #[test]
    fn interface_with_final_rejected() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Bad",
            None,
            ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT | ClassAccessFlags::FINAL,
            vec![],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_err());
    }

    #[test]
    fn valid_class_passes() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Good",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_ok());
    }

    #[test]
    fn valid_interface_passes() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "GoodInterface",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT,
            vec![],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_ok());
    }

    // --- Abstract method flag checks ---

    #[test]
    fn abstract_private_method_rejected() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Bad",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
            vec![make_method(
                "foo",
                "()V",
                MethodAccessFlags::ABSTRACT | MethodAccessFlags::PRIVATE,
                false,
            )],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_err());
    }

    #[test]
    fn abstract_static_method_rejected() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Bad",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
            vec![make_method(
                "foo",
                "()V",
                MethodAccessFlags::ABSTRACT | MethodAccessFlags::STATIC,
                false,
            )],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_err());
    }

    // --- Final class constraint ---

    #[test]
    fn extend_final_class_rejected() {
        let mut store = ClassStore::new();
        let parent_id = make_class_in_store(
            &mut store,
            "FinalParent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL | ClassAccessFlags::SUPER,
            vec![],
        );
        let child_id = make_class_in_store(
            &mut store,
            "Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![],
        );
        let child = store.get(child_id).unwrap();
        assert!(verify_class_structure(child, &store).is_err());
    }

    // --- Final method constraint ---

    #[test]
    fn override_final_method_rejected() {
        let mut store = ClassStore::new();
        let parent_id = make_class_in_store(
            &mut store,
            "Parent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "foo",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL,
                true,
            )],
        );
        let child_id = make_class_in_store(
            &mut store,
            "Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
        );
        let child = store.get(child_id).unwrap();
        assert!(verify_class_structure(child, &store).is_err());
    }

    #[test]
    fn override_non_final_method_ok() {
        let mut store = ClassStore::new();
        let parent_id = make_class_in_store(
            &mut store,
            "Parent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
        );
        let child_id = make_class_in_store(
            &mut store,
            "Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
        );
        let child = store.get(child_id).unwrap();
        assert!(verify_class_structure(child, &store).is_ok());
    }

    #[test]
    fn private_method_shadowing_final_method_ok() {
        // HotSpot links a class declaring its own private
        // getClass()Ljava/lang/Class; although Object.getClass is final.
        let mut store = ClassStore::new();
        let object_id = make_class_in_store(
            &mut store,
            "java/lang/Object",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "getClass",
                "()Ljava/lang/Class;",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL | MethodAccessFlags::NATIVE,
                false,
            )],
        );
        let child_id = make_class_in_store(
            &mut store,
            "GetClassPrivate",
            Some(object_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "getClass",
                "()Ljava/lang/Class;",
                MethodAccessFlags::PRIVATE,
                true,
            )],
        );
        let child = store.get(child_id).unwrap();
        assert!(verify_class_structure(child, &store).is_ok());
    }

    #[test]
    fn public_method_overriding_object_get_class_still_rejected() {
        let mut store = ClassStore::new();
        let object_id = make_class_in_store(
            &mut store,
            "java/lang/Object",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "getClass",
                "()Ljava/lang/Class;",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL | MethodAccessFlags::NATIVE,
                false,
            )],
        );
        let child_id = make_class_in_store(
            &mut store,
            "p/GetClassPublic",
            Some(object_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "getClass",
                "()Ljava/lang/Class;",
                MethodAccessFlags::PUBLIC,
                true,
            )],
        );
        let child = store.get(child_id).unwrap();
        match verify_class_structure(child, &store) {
            Err(LinkageError::IncompatibleClassChangeError { message }) => assert_eq!(
                message,
                "class p.GetClassPublic overrides final method \
                 java.lang.Object.getClass()Ljava/lang/Class;"
            ),
            other => panic!("expected IncompatibleClassChangeError, got {other:?}"),
        }
    }

    #[test]
    fn final_method_found_behind_non_final_intermediate_rejected() {
        // HotSpot continues the search above a non-final same-signature
        // declaration; the final one further up is still an illegal override.
        let mut store = ClassStore::new();
        let top_id = make_class_in_store(
            &mut store,
            "p/Top",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "foo",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL,
                true,
            )],
        );
        let mid_id = make_class_in_store(
            &mut store,
            "p/Mid",
            Some(top_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::PRIVATE, true)],
        );
        let child_id = make_class_in_store(
            &mut store,
            "q/Child",
            Some(mid_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
        );
        let child = store.get(child_id).unwrap();
        assert!(verify_final_method_constraint(child, &store).is_err());
    }

    #[test]
    fn package_private_final_across_packages_ok() {
        let mut store = ClassStore::new();
        let parent_id = make_class_in_store(
            &mut store,
            "p/Parent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::FINAL, true)],
        );
        let other_pkg = make_class_in_store(
            &mut store,
            "q/Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
        );
        let same_pkg = make_class_in_store(
            &mut store,
            "p/Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, true)],
        );
        assert!(verify_final_method_constraint(store.get(other_pkg).unwrap(), &store).is_ok());
        assert!(verify_final_method_constraint(store.get(same_pkg).unwrap(), &store).is_err());
    }

    // --- Abstract method implementation ---

    #[test]
    fn concrete_class_missing_abstract_impl_rejected() {
        let mut store = ClassStore::new();
        let parent_id = make_class_in_store(
            &mut store,
            "AbstractParent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
            vec![make_method(
                "doThing",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
                false,
            )],
        );
        let child_id = make_class_in_store(
            &mut store,
            "ConcreteChild",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![], // no implementation of doThing
        );
        let child = store.get(child_id).unwrap();
        // The FOCUSED checker still rejects the shape...
        assert!(verify_inherited_abstract_methods_implemented(child, &store).is_err());
        // ...but ordinary class loading deliberately does NOT enforce it
        // (HotSpot parity: linkage proceeds and AbstractMethodError is
        // raised only if the missing method is actually invoked — see the
        // `let _ = verify_inherited_abstract_methods_implemented;` note in
        // `verify_class_structure`; JAXB's `JAXBContextImpl`, which leaves
        // the deprecated abstract `createValidator()` unimplemented, is a
        // real-world class that must keep loading). This assertion pins the
        // deliberate non-enforcement so a future re-enable is a conscious
        // choice.
        assert!(verify_class_structure(child, &store).is_ok());
    }

    #[test]
    fn concrete_class_with_abstract_impl_ok() {
        let mut store = ClassStore::new();
        let parent_id = make_class_in_store(
            &mut store,
            "AbstractParent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
            vec![make_method(
                "doThing",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
                false,
            )],
        );
        let child_id = make_class_in_store(
            &mut store,
            "ConcreteChild",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "doThing",
                "()V",
                MethodAccessFlags::PUBLIC,
                true,
            )],
        );
        let child = store.get(child_id).unwrap();
        assert!(verify_class_structure(child, &store).is_ok());
    }

    #[test]
    fn abstract_class_may_skip_abstract_impl() {
        let mut store = ClassStore::new();
        let parent_id = make_class_in_store(
            &mut store,
            "AbstractParent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
            vec![make_method(
                "doThing",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
                false,
            )],
        );
        let child_id = make_class_in_store(
            &mut store,
            "AbstractChild",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
            vec![], // abstract child need not implement
        );
        let child = store.get(child_id).unwrap();
        assert!(verify_class_structure(child, &store).is_ok());
    }

    // --- Code attribute presence ---

    #[test]
    fn concrete_method_without_code_rejected() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Bad",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method("foo", "()V", MethodAccessFlags::PUBLIC, false)],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_err());
    }

    #[test]
    fn abstract_method_with_code_rejected() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Bad",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT | ClassAccessFlags::SUPER,
            vec![make_method(
                "foo",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
                true, // should not have Code
            )],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_err());
    }

    #[test]
    fn native_method_with_code_rejected() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Bad",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "nativeMethod",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                true, // should not have Code
            )],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_err());
    }

    #[test]
    fn native_method_without_code_ok() {
        let mut store = ClassStore::new();
        let id = make_class_in_store(
            &mut store,
            "Good",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            vec![make_method(
                "nativeMethod",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                false,
            )],
        );
        let class = store.get(id).unwrap();
        assert!(verify_class_structure(class, &store).is_ok());
    }

    // -----------------------------------------------------------------
    // F3 — JSR/RET subroutine relaxation (Pass 3, JVMS §4.10.2.5)
    // -----------------------------------------------------------------

    /// Mock hierarchy used by the JSR/RET tests below — every class is a
    /// subclass of every other, common superclass is always Object,
    /// nothing is an interface. Sufficient because the JSR-relaxation
    /// path does no type-state checking that would consult the
    /// hierarchy.
    struct PermissiveHierarchy;

    impl crate::vtype::ClassHierarchy for PermissiveHierarchy {
        fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
            true
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    fn make_pre_java7_jsr_class(
        method_name: &str,
        descriptor: &str,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
        exception_table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
    ) -> Class {
        Class {
            id: ClassId::new(0),
            loader_id: ClassLoaderId::Application,
            name: Arc::from("Hierarchical"),
            source_file: None,
            // Java 5 (major 49) — pre-Java-7, no StackMapTable
            // requirement; legitimately may emit jsr/ret.
            version: ClassFileVersion::JAVA_5,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_cp(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC,
                name: Arc::from(method_name),
                descriptor: Arc::from(descriptor),
                attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                    CodeAttribute {
                        max_stack,
                        max_locals,
                        code: cratonvm_reader::ByteView::from_vec(code),
                        exception_table,
                        attributes: vec![],
                    },
                )))],
            }],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: crate::class_origin::ClassOrigin::VmInternal,
            has_finalizer: false,
            signature: None,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        }
    }

    /// A synthetic method that mirrors the
    /// `TypePool$AbstractBase$Hierarchical.clear` shape from
    /// ByteBuddy 1.12.12 — two distinct `jsr` call sites that target
    /// the same subroutine. Our worklist verifier cannot type-check it
    /// (it merges `ReturnAddress(3)` and `ReturnAddress(6)` into `Top`).
    ///
    /// Version-gated policy: at a **legal pre-Java-7 version (major 49)** the
    /// opcodes are permitted (JVMS §4.9.1) and HotSpot loads the class, so
    /// CratonVM accepts it after the structural scan — by default, with no
    /// escape hatch. The escape hatch is exercised here only to confirm it is
    /// also a path to acceptance.
    ///
    /// Bytecode:
    /// ```text
    ///   0: jsr +9      → push returnAddress(3),  branch to 9
    ///   3: jsr +6      → push returnAddress(6),  branch to 9
    ///   6: return
    ///   7: nop          (alignment padding)
    ///   8: nop
    ///   9: astore_0     ← subroutine entry: store the merged returnAddress
    ///  10: ret 0        ← return via the stored address
    /// ```
    #[test]
    fn jsr_double_call_site_pre_java7_accepted_by_default() {
        let class = make_pre_java7_jsr_class(
            "clear",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x09, // 0: jsr +9
                0xa8, 0x00, 0x06, // 3: jsr +6
                0xb1, // 6: return
                0x00, 0x00, // 7..8: nop padding (unreachable)
                0x4b, // 9: astore_0
                0xa9, 0x00, // 10: ret 0
            ],
            vec![],
        );
        // Default policy (escape hatch off): legal pre-Java-7 version, so the
        // structurally well-formed subroutine method is accepted — this is the
        // regression fix (ByteBuddy `JavaDispatcher$DynamicClassLoader.proxy`).
        let accepted_default =
            verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
        assert!(
            accepted_default.is_ok(),
            "legal pre-Java-7 (major 49) subroutine method must be accepted by \
             default (HotSpot loads it), got {accepted_default:?}"
        );
        // Escape hatch on: also accepted (structural-only).
        let accepted_hatch = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(
            accepted_hatch.is_ok(),
            "with CRATONVM_ALLOW_JSR_RET the structurally-valid subroutine method \
             must be accepted (structural-only), got {accepted_hatch:?}"
        );
    }

    // -----------------------------------------------------------------------
    // TYPE MAPS on the subroutine path (arch-2026-07-26)
    // -----------------------------------------------------------------------

    /// Build a pre-Java-7 class carrying an explicit `ClassId` and an arbitrary
    /// method list.
    ///
    /// The type-map store is a process-wide side table with first-writer-wins
    /// install semantics, so every test that publishes must own a distinct
    /// `ClassId` or it will read another test's maps under `cargo test`'s
    /// parallel harness. The 90_00x range is reserved for these tests.
    fn make_pre_java7_class_with_id(raw_id: u32, methods: Vec<ClassFileMethod>) -> Class {
        let mut class = make_pre_java7_jsr_class("placeholder", "()V", 1, 1, vec![0xb1], vec![]);
        class.id = ClassId::new(raw_id);
        class.methods = methods;
        class
    }

    /// A method with a `Code` attribute and no subroutine opcodes.
    fn plain_method(name: &str, code: Vec<u8>, max_stack: u16, max_locals: u16) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from(name),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }
    }

    /// The double-`jsr` body used by the acceptance tests above.
    fn double_jsr_method(name: &str) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from(name),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![
                        0xa8, 0x00, 0x09, // 0: jsr +9
                        0xa8, 0x00, 0x06, // 3: jsr +6
                        0xb1, // 6: return
                        0x00, 0x00, // 7..8: nop padding
                        0x4b, // 9: astore_0
                        0xa9, 0x00, // 10: ret 0
                    ]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }
    }

    /// REGRESSION: a single legacy `finally` used to cost the **whole class**
    /// its type maps.
    ///
    /// `verify_class_bytecode_inner` routes a subroutine-bearing class down its
    /// own per-method path, which never calls
    /// `bytecode_verifier::verify_bytecode` — the only place that used to
    /// publish. So `verification_status` answered `Unknown` and *every* method,
    /// including the ordinary ones that were fully type-state verified, fell
    /// back to conservative scanning. It must now publish.
    #[test]
    fn subroutine_bearing_class_publishes_maps_for_its_non_jsr_methods() {
        let class = make_pre_java7_class_with_id(
            90_001,
            vec![
                // aload_0; astore_0; return — no branches, no handlers, no jsr.
                plain_method("plain", vec![0x2a, 0x4b, 0xb1], 1, 1),
                double_jsr_method("legacyFinally"),
            ],
        );
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
        assert!(
            res.is_ok(),
            "legal pre-Java-7 class must verify, got {res:?}"
        );

        assert_eq!(
            crate::type_maps::verification_status(class.id),
            crate::type_maps::VerificationStatus::Verified,
            "a subroutine-bearing class must no longer answer Unknown"
        );

        let plain = crate::type_maps::type_maps_for(class.id, 0)
            .expect("the non-jsr method must have maps");
        // pc 0 is an instruction start with a proven layout.
        let locals = plain.local_oops_at(0).expect("pc 0 must be described");
        assert!(
            locals.get(0),
            "local 0 of an instance method is `this`, a reference"
        );
        assert_eq!(
            plain.stack_depth_at(0),
            Some(0),
            "the operand stack is empty on entry"
        );
        // pc 1 is after `aload_0`: one reference on the stack.
        assert_eq!(plain.stack_depth_at(1), Some(1));
        assert!(
            plain
                .stack_oops_at(1)
                .expect("pc 1 must be described")
                .get(0),
            "the value `aload_0` pushed is a reference"
        );
        assert!(
            plain.safe_for_fast_path(),
            "a straight-line method with in-range local operands is fast-path safe; \
             veto was {:?}",
            plain.fast_path_veto()
        );
    }

    /// The subroutine method itself stays conservative — but *explicitly*.
    ///
    /// The contract is that `None` from `oop_map_at` means "unproven", never
    /// "no references live here". A fabricated all-clear bitmap would be a
    /// missed-root bug (JVMS §4.10.2.5 subroutine inlining exists precisely
    /// because a naive merge collapses a caller's live reference to `Top` for
    /// the duration of the subroutine, and that slot is read as a reference
    /// again after `ret`). So the method publishes a *row-less* map whose veto
    /// names the reason.
    #[test]
    fn jsr_method_map_is_explicitly_unproven_not_fabricated_empty() {
        let class = make_pre_java7_class_with_id(
            90_002,
            vec![
                double_jsr_method("legacyFinally"),
                plain_method("plain", vec![0xb1], 1, 1),
            ],
        );
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
        assert!(
            res.is_ok(),
            "legal pre-Java-7 class must verify, got {res:?}"
        );

        let jsr = crate::type_maps::type_maps_for(class.id, 0)
            .expect("the jsr method must have an explicit entry, not silence");
        assert_eq!(
            jsr.entry_count(),
            0,
            "no row may be fabricated for a method whose type-state is unproven"
        );
        for pc in 0..12u32 {
            assert!(
                jsr.oop_map_at(pc).is_none(),
                "pc {pc} must answer `None` (unproven → scan conservatively)"
            );
            assert!(jsr.local_oops_at(pc).is_none());
            assert!(jsr.stack_oops_at(pc).is_none());
        }
        assert!(
            !jsr.safe_for_fast_path(),
            "a subroutine method must never take the unchecked interpreter path"
        );
        assert_eq!(
            jsr.fast_path_veto(),
            Some(FastPathVeto::Subroutine),
            "the veto reason is what makes the conservative fallback diagnosable \
             rather than indistinguishable from an unverified class"
        );
    }

    /// Abstract and native methods must still occupy their position so
    /// `ClassTypeMaps` indices line up with `Class::methods`.
    #[test]
    fn subroutine_class_type_map_indices_track_class_methods() {
        let class = make_pre_java7_class_with_id(
            90_003,
            vec![
                make_method("abs", "()V", MethodAccessFlags::ABSTRACT, false),
                double_jsr_method("legacyFinally"),
                make_method("nat", "()V", MethodAccessFlags::NATIVE, false),
                plain_method("plain", vec![0x2a, 0x4b, 0xb1], 1, 1),
            ],
        );
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
        assert!(res.is_ok(), "class must verify, got {res:?}");

        let maps = crate::type_maps::class_type_maps(class.id).expect("maps must be published");
        assert_eq!(maps.method_count(), 4, "one entry per Class::methods slot");
        assert!(maps.method(0).is_none(), "abstract method has no body");
        assert_eq!(
            maps.method(1).and_then(|m| m.fast_path_veto()),
            Some(FastPathVeto::Subroutine)
        );
        assert!(maps.method(2).is_none(), "native method has no body");
        assert!(
            maps.method(3).is_some_and(|m| m.entry_count() > 0),
            "the plain method at index 3 must carry real rows"
        );
        // The by-name path must agree with the by-index path.
        assert_eq!(
            crate::type_maps::type_maps_for_named(class.id, "plain", "()V")
                .map(|m| m.entry_count()),
            maps.method(3).map(|m| m.entry_count())
        );
    }

    /// A class that fails verification must publish nothing: a half-built map
    /// for a class that never loads would be visible to the GC forever.
    #[test]
    fn failed_subroutine_class_publishes_no_maps() {
        let mut class =
            make_pre_java7_class_with_id(90_004, vec![double_jsr_method("legacyFinally")]);
        // Major ≥ 51 makes the subroutine opcodes illegal (JVMS §4.9.1).
        class.version = ClassFileVersion::JAVA_7;
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
        assert!(res.is_err(), "subroutine at major 51 must be rejected");
        assert_eq!(
            crate::type_maps::verification_status(class.id),
            crate::type_maps::VerificationStatus::Unknown,
            "a class that failed verification must leave no maps behind"
        );
    }

    /// The same double-`jsr` shape, but stamped with a **Java 7+ class-file
    /// version (major 51)** where JVMS §4.9.1 *forbids* `jsr`/`jsr_w`/`ret`.
    /// Such a class is malformed; HotSpot rejects it and so must CratonVM by
    /// default. The `CRATONVM_ALLOW_JSR_RET` escape hatch still forces
    /// structural-only acceptance for deployments that must load such jars.
    #[test]
    fn jsr_double_call_site_java7plus_rejected_by_default_accepted_with_escape_hatch() {
        let mut class = make_pre_java7_jsr_class(
            "clear",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x09, // 0: jsr +9
                0xa8, 0x00, 0x06, // 3: jsr +6
                0xb1, // 6: return
                0x00, 0x00, // 7..8: nop padding (unreachable)
                0x4b, // 9: astore_0
                0xa9, 0x00, // 10: ret 0
            ],
            vec![],
        );
        // Forbidden at major ≥ 51 — mark the class as Java 7.
        class.version = ClassFileVersion::JAVA_7;
        // Default policy: malformed (subroutine opcode at version ≥ 51) → reject.
        let rejected = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
        assert!(
            rejected.is_err(),
            "subroutine opcode at class-file version 51+ is forbidden (JVMS §4.9.1) \
             and must be rejected by default, got {rejected:?}"
        );
        // Escape hatch on: structural-only acceptance regardless of version.
        let accepted = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(
            accepted.is_ok(),
            "with CRATONVM_ALLOW_JSR_RET the structurally-valid subroutine method \
             must be accepted even at version 51+, got {accepted:?}"
        );
    }

    /// F3 closer match to the real `clear()` shape: jsr inside a
    /// `try-finally` whose `catch` re-jsr's to the same subroutine.
    /// The exception table makes offset 15 (`astore_1`) reachable from
    /// the protected range as a handler entry — this mirrors the
    /// `Exception table: 0 12 15 any; 15 19 15 any` layout from the
    /// ByteBuddy class file.
    ///
    /// Bytecode:
    /// ```text
    ///   0: aconst_null  (placeholder for the parent.clear() effect)
    ///   1: pop
    ///   2: jsr +19      → push returnAddress(5),  branch to 21
    ///   5: goto 28
    ///   8: nop padding
    ///   9: nop
    ///  10: nop
    ///  11: nop
    ///  12: nop
    ///  13: nop
    ///  14: nop
    ///  15: astore_1     ← handler entry: store the throwable
    ///  16: jsr +5       → push returnAddress(19),  branch to 21
    ///  19: aload_1
    ///  20: athrow
    ///  21: astore_2     ← subroutine entry
    ///  22: aload_0      (filler)
    ///  23: pop
    ///  24: nop
    ///  25: nop
    ///  26: ret 2
    ///  28: return
    /// ```
    /// The exception table protects [0..15) with handler at 15, which
    /// in real bytecode would be the `try { parent.clear() } catch (any)`
    /// shape that the Java 5 javac emits for `try-finally`. At this legal
    /// pre-Java-7 version the class is accepted by default (HotSpot loads it).
    #[test]
    fn jsr_finally_handler_double_call_site_pre_java7_accepted_by_default() {
        // Build the bytecode array first so we can reference precise
        // offsets in the exception table without juggling magic numbers.
        let code: Vec<u8> = vec![
            0x01, // 0: aconst_null
            0x57, // 1: pop
            0xa8, 0x00, 0x13, // 2: jsr +19  → 21
            0xa7, 0x00, 0x17, // 5: goto +23 → 28
            0x00, 0x00, 0x00, // 8..10: nop
            0x00, 0x00, 0x00, // 11..13: nop
            0x00, // 14: nop
            0x4c, // 15: astore_1
            0xa8, 0x00, 0x05, // 16: jsr +5 → 21
            0x2b, // 19: aload_1
            0xbf, // 20: athrow
            0x4d, // 21: astore_2
            0x2a, // 22: aload_0
            0x57, // 23: pop
            0x00, 0x00, // 24..25: nop
            0xa9, 0x02, // 26: ret 2
            0xb1, // 28: return
        ];
        let exc = vec![
            cratonvm_reader::attribute::ExceptionTableEntry {
                start_pc: 0,
                end_pc: 15,
                handler_pc: 15,
                catch_type: 0, // any
            },
            cratonvm_reader::attribute::ExceptionTableEntry {
                start_pc: 15,
                end_pc: 19,
                handler_pc: 15,
                catch_type: 0,
            },
        ];
        let class = make_pre_java7_jsr_class("clear", "()V", 1, 3, code, exc);
        // Legal pre-Java-7 (major 49) version: HotSpot loads this try-finally
        // double-jsr shape, so CratonVM accepts it after the structural scan —
        // by default, no escape hatch required.
        let accepted_default =
            verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false);
        assert!(
            accepted_default.is_ok(),
            "ByteBuddy clear()-shape (try-finally with double-jsr to one \
             subroutine) at a legal pre-Java-7 version must be accepted by \
             default, got {accepted_default:?}"
        );
        // Escape hatch: also accepted (structural-only).
        let accepted_hatch = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(
            accepted_hatch.is_ok(),
            "with CRATONVM_ALLOW_JSR_RET the structurally-valid clear()-shape \
             must be accepted (structural-only), got {accepted_hatch:?}"
        );
    }

    /// Sanity: structural validation still rejects malformed bytecode
    /// in JSR-using methods. A `jsr` whose 16-bit offset points past
    /// the end of the bytecode array is rejected by the structural
    /// fallback. Exercised with the escape hatch ON so the failure is
    /// attributable to the structural pass and not to the default
    /// subroutine rejection — the structural scan must still run (and
    /// reject) even when JSR methods are tolerated.
    #[test]
    fn jsr_with_out_of_range_target_is_rejected() {
        let class = make_pre_java7_jsr_class(
            "bad",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x7f, 0xff, // jsr +32767 (way past end of method)
                0xb1, // return
            ],
            vec![],
        );
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(
            res.is_err(),
            "out-of-range jsr target must still be rejected by structural pass \
             even with the escape hatch on, got {res:?}"
        );
    }

    /// Sanity: a class whose pre-Java-7 method does NOT use jsr/ret
    /// continues to be type-state-verified. Here the method has a stack
    /// underflow (`ireturn` on an empty stack) — the standard verifier
    /// must catch it because we never enter the JSR-relaxation branch.
    #[test]
    fn non_jsr_pre_java7_method_still_strictly_verified() {
        let class = make_pre_java7_jsr_class(
            "bad",
            "()I",
            0,
            0,
            vec![0xac], // ireturn on empty stack
            vec![],
        );
        let res = verify_class_bytecode(&class, &PermissiveHierarchy);
        assert!(
            res.is_err(),
            "non-JSR method must continue to be strictly type-state-verified, \
             got {res:?}"
        );
    }

    /// Sanity: when a JSR method (tolerated under the escape hatch)
    /// appears BEFORE a non-JSR method with a type-state bug, the
    /// non-JSR failure must still be re-detected — tolerating the JSR
    /// method must not short-circuit verification of the rest of the
    /// class. Exercised with the escape hatch ON so the JSR method is
    /// accepted (structural-only) rather than triggering the default
    /// rejection, isolating the non-JSR bug as the cause of failure.
    #[test]
    fn failing_jsr_method_does_not_mask_later_real_bug() {
        // Build the ByteBuddy clear() shape (which fails the worklist
        // verifier without our fix) followed by a non-JSR method with
        // a stack-underflow type-state bug.
        let mut class = make_pre_java7_jsr_class(
            "clear",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x09, // 0: jsr +9
                0xa8, 0x00, 0x06, // 3: jsr +6
                0xb1, // 6: return
                0x00, 0x00, // 7..8: pad
                0x4b, // 9: astore_0
                0xa9, 0x00, // 10: ret 0
            ],
            vec![],
        );
        class.methods.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("typeStateBug"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xac]), // ireturn on empty stack
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        });
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(
            res.is_err(),
            "non-JSR type-state bug must be detected even when a tolerated \
             JSR method precedes it, got {res:?}"
        );
    }

    /// Sanity: when a JSR method (tolerated under the escape hatch)
    /// appears BEFORE a non-JSR method with a type-state bug (stack
    /// underflow), the non-JSR method's bug must still be caught.
    /// Exercised with the escape hatch ON: the JSR method is accepted
    /// structurally, so the failure can only come from the non-JSR bug —
    /// proving the per-method routing still type-state-verifies siblings.
    #[test]
    fn type_state_bug_after_jsr_method_still_detected() {
        let mut class = make_pre_java7_jsr_class(
            "subroutineMethod",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x06, // 0: jsr +6 → 6
                0xb1, // 3: return
                0x00, 0x00, // 4..5: pad
                0x4b, // 6: astore_0
                0xa9, 0x00, // 7: ret 0
            ],
            vec![],
        );
        // Append a non-JSR method with a type-state error (ireturn on
        // empty stack — passes structural check but fails type-state).
        class.methods.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("typeStateBug"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xac]), // ireturn on empty stack
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        });
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(
            res.is_err(),
            "type-state bug in non-JSR method must be caught even when a \
             JSR method precedes it in class.methods, got {res:?}"
        );
    }

    /// Sanity: a class with a mix of JSR-using and non-JSR methods
    /// applies the structural-only tolerance only to the JSR ones (under
    /// the escape hatch). The non-JSR method here is malformed (truncated
    /// bytecode for `getstatic`) and must be rejected even though another
    /// method in the same class uses `jsr`. Exercised with the escape
    /// hatch ON so the failure is attributable to the malformed non-JSR
    /// method, not the default subroutine rejection.
    #[test]
    fn mixed_jsr_and_non_jsr_methods_isolate_relaxation() {
        let mut class = make_pre_java7_jsr_class(
            "subroutineMethod",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x06, // 0: jsr +6 → 6
                0xb1, // 3: return
                0x00, 0x00, // 4..5: pad
                0x4b, // 6: astore_0
                0xa9, 0x00, // 7: ret 0
            ],
            vec![],
        );
        // Add a second method that does NOT use jsr/ret but is
        // structurally malformed (truncated `getstatic` — opcode 0xb2
        // wants 2 operand bytes; we only supply 1).
        class.methods.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("malformed"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xb2, 0x00]), // getstatic with truncated index
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        });
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(
            res.is_err(),
            "malformed non-JSR method in a mixed class must still be rejected, \
             got {res:?}"
        );
    }

    /// SECURITY FIX (HIGH): the default-policy rejection of a
    /// subroutine-using method at a *forbidden* class-file version
    /// (major ≥ 51) must surface as a `VerifyError` whose message names
    /// the `jsr/jsr_w/ret` cause and the `CRATONVM_ALLOW_JSR_RET` escape
    /// hatch, so the refusal is diagnosable rather than an opaque failure.
    #[test]
    fn jsr_default_rejection_is_a_verify_error_with_diagnostic() {
        let mut class = make_pre_java7_jsr_class(
            "clear",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x06, // 0: jsr +6 → 6
                0xb1, // 3: return
                0x00, 0x00, // 4..5: pad
                0x4b, // 6: astore_0
                0xa9, 0x00, // 7: ret 0
            ],
            vec![],
        );
        // Java 7+ (major 51): subroutine opcodes are forbidden → rejected by
        // default with a diagnostic message.
        class.version = ClassFileVersion::JAVA_7;
        match verify_class_bytecode_inner(&class, &PermissiveHierarchy, false, false) {
            Err(LinkageError::VerifyError {
                method_name,
                message,
                ..
            }) => {
                assert_eq!(method_name, "clear");
                assert!(
                    message.contains("jsr") && message.contains("CRATONVM_ALLOW_JSR_RET"),
                    "rejection message must name the jsr/ret cause and the escape \
                     hatch, got: {message}"
                );
            }
            other => panic!("expected VerifyError for default jsr rejection, got {other:?}"),
        }
    }

    fn assert_legacy_jsr_structural_rejects(code: Vec<u8>, max_locals: u16, needle: &str) {
        let class = make_pre_java7_jsr_class("legacy", "()V", 2, max_locals, code, vec![]);
        match verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false) {
            Err(LinkageError::VerifyError { message, .. }) => {
                assert!(
                    message.contains(needle),
                    "expected VerifyError containing '{needle}', got: {message}"
                );
            }
            other => panic!("expected structural VerifyError, got {other:?}"),
        }
    }

    #[test]
    fn legacy_jsr_structural_rejects_negative_target() {
        assert_legacy_jsr_structural_rejects(
            vec![
                0xa8, 0xff, 0xff, // 0: jsr -1 -> negative target
                0xb1, // 3: return
            ],
            1,
            "Illegal target of jump or branch",
        );
    }

    #[test]
    fn legacy_jsr_structural_rejects_code_len_target() {
        assert_legacy_jsr_structural_rejects(
            vec![
                0xa8, 0x00, 0x04, // 0: jsr +4 -> code_len
                0xb1, // 3: return
            ],
            1,
            "Illegal target of jump or branch",
        );
    }

    #[test]
    fn legacy_jsr_structural_rejects_huge_wide_target() {
        assert_legacy_jsr_structural_rejects(
            vec![
                0xc9, 0x7f, 0xff, 0xff, 0xff, // 0: jsr_w i32::MAX
                0xb1, // 5: return
            ],
            1,
            "Illegal target of jump or branch",
        );
    }

    #[test]
    fn legacy_jsr_structural_rejects_non_instruction_target() {
        assert_legacy_jsr_structural_rejects(
            vec![
                0xa8, 0x00, 0x01, // 0: jsr +1 -> operand byte, not opcode
                0xb1, // 3: return
            ],
            1,
            "Illegal target of jump or branch",
        );
    }

    #[test]
    fn legacy_ret_structural_rejects_uncovered_local() {
        assert_legacy_jsr_structural_rejects(
            vec![
                0xa9, 0x00, // 0: ret 0 without a reachable jsr/astore
            ],
            1,
            // Wave 104 hand-back: the subroutine walk answers the uncovered
            // `ret` first, with libverify's register check (register 0 holds
            // `this`, not a return address).
            "(class: Hierarchical, method: legacy signature: ()V) Register 0 contains wrong type",
        );
    }

    /// Wave 105 hand-back (lane L7, probe `L7W105JsrPcKeyedMerge` R5): a
    /// subroutine B called from top level AND from inside A has the
    /// intersected mask list [B]; B's `ret` returns with the RET state's list
    /// cut below B (libverify's `new_mask_count = i`), i.e. [], so A's `ret`
    /// finds A missing, as HotSpot 25 answers.
    #[test]
    fn shared_subroutine_orphans_the_outer_ret() {
        assert_legacy_jsr_structural_rejects(
            vec![
                0x03, 0x3c, // 0: iconst_0; 1: istore_1
                0xa8, 0x00, 0x0f, // 2: jsr 17 (B)
                0xa8, 0x00, 0x04, // 5: jsr 9 (A)
                0xb1, // 8: return
                0x4d, 0x01, 0x4c, // A 9: astore_2; 10: aconst_null; 11: astore_1
                0xa8, 0x00, 0x05, // 12: jsr 17 (B, from inside A)
                0xa9, 0x02, // 15: ret 2
                0x4e, 0x00, 0x00, // B 17: astore_3; 18: nop; 19: nop
                0xa9, 0x03, // 20: ret 3
            ],
            4,
            "(class: Hierarchical, method: legacy signature: ()V) Illegal return from subroutine",
        );
    }

    /// Control for the test above: javac's nested `finally` (B called only
    /// from inside A) verifies -- B's `ret` returns into A with [A]. The
    /// structural scan's coverage rule misses A's `ret 1` at 8 (it sits after
    /// the inner `jsr`); the walk checks the whole method and accepts, which
    /// overrules that coverage-only error (second wave-105 hand-back).
    #[test]
    fn nested_subroutine_returns_into_its_caller() {
        let class = make_pre_java7_jsr_class(
            "legacy",
            "()V",
            2,
            3,
            vec![
                0xa8, 0x00, 0x04, // 0: jsr 4 (A)
                0xb1, // 3: return
                0x4c, // A 4: astore_1
                0xa8, 0x00, 0x05, // 5: jsr 10 (B)
                0xa9, 0x01, // 8: ret 1
                0x4d, // B 10: astore_2
                0xa9, 0x02, // 11: ret 2
            ],
            vec![],
        );
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(res.is_ok(), "javac's nested finally must verify, got {res:?}");
    }

    #[test]
    fn legacy_ret_structural_rejects_local_out_of_range() {
        assert_legacy_jsr_structural_rejects(
            vec![
                0xa9, 0x01, // 0: ret 1 with max_locals = 1
            ],
            1,
            "Illegal local variable number",
        );
    }

    /// Sanity: the JSR scanner must not be fooled by a `tableswitch`
    /// whose jump-table contents happen to contain the byte `0xa8`
    /// (the `jsr` opcode). The instruction-level walk steps over the
    /// switch's variable-length operand correctly.
    #[test]
    fn jsr_scanner_handles_tableswitch_padding() {
        // Layout: aligned `tableswitch` whose default and offsets
        // include byte values that would be misread as jsr if the
        // scanner walked one byte at a time. We only assert that the
        // method classifies as non-JSR (so it should reach the strict
        // verifier and fail there if at all).
        //
        //   0: iconst_0      (0x03)
        //   1: tableswitch   (0xaa)
        //   2..3: pad (to 4-byte align after opcode at offset 1: pad to offset 4)
        //   4..7: default = 16
        //   8..11: low = 0
        //  12..15: high = 0
        //  16..19: offset[0] = 12  ← the byte 0x0c, no 0xa8
        //  20: ireturn (0xac)
        let mut code: Vec<u8> = Vec::new();
        code.push(0x03); // 0: iconst_0
        code.push(0xaa); // 1: tableswitch
                         // Pad so default starts at offset (1+1+pad) ≡ 0 mod 4 → next offset must be 4
                         // so we need 2 pad bytes after offset 1
        code.push(0x00);
        code.push(0x00);
        // 4..7: default
        code.extend(&12i32.to_be_bytes());
        // 8..11: low
        code.extend(&0i32.to_be_bytes());
        // 12..15: high
        code.extend(&0i32.to_be_bytes());
        // 16..19: offset[0]
        code.extend(&12i32.to_be_bytes());
        // 20: ireturn (0xac)
        code.push(0xac);

        // Wrap it in the same Class scaffolding.
        let method = ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("withSwitch"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        };
        // Scanner-level assertion only — we don't care whether the full
        // verifier accepts the synthetic switch.
        assert!(
            !method_uses_jsr_or_ret(&method),
            "tableswitch padding must not be misclassified as jsr"
        );
    }

    /// Targeted unit test for the `is_pre_java7` helper.
    #[test]
    fn pre_java7_classification() {
        assert!(is_pre_java7(&ClassFileVersion::JAVA_5));
        assert!(is_pre_java7(&ClassFileVersion::JAVA_6));
        assert!(!is_pre_java7(&ClassFileVersion::JAVA_7));
        assert!(!is_pre_java7(&ClassFileVersion::JAVA_8));
        assert!(!is_pre_java7(&ClassFileVersion::JAVA_25));
    }

    // -----------------------------------------------------------------
    // MED — invokespecial <init> new-site owner-match (JVMS §4.10.1.9)
    // -----------------------------------------------------------------

    /// Constant pool whose entry #6 is a `Methodref` to `D.<init>()V`.
    fn init_owner_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,                        // 0
            ConstantPoolEntry::Utf8("D".into()),                 // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2 (owner D)
            ConstantPoolEntry::Utf8("<init>".into()),            // 3
            ConstantPoolEntry::Utf8("()V".into()),               // 4
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6 (D.<init>()V)
        ])
    }

    fn owner_match_frame() -> VerificationFrame {
        // A frame with a single `Uninitialized(0)` receiver on the stack.
        let mut frame = VerificationFrame::initial_frame("Test", "test", "()V", true, 1, 4);
        frame.push(VType::Uninitialized(0)).unwrap();
        frame
    }

    #[test]
    fn invokespecial_init_owner_mismatch_rejected() {
        // `new C` (recorded at offset 0) followed by `invokespecial D.<init>`
        // is a JVMS §4.10.1.9 violation: the new-site type C must equal the
        // constructor owner D.
        let cp = init_owner_cp();
        let frame = owner_match_frame();
        let mut new_site_classes = std::collections::HashMap::new();
        new_site_classes.insert(0u16, std::sync::Arc::<str>::from("C")); // new C, not D

        let res = check_new_init_owner_match(
            &Instruction::Invokespecial(6),
            &frame,
            &cp,
            &new_site_classes,
            "Test",
            "test",
        );
        assert!(
            res.is_err(),
            "invokespecial D.<init> on an object created by `new C` must be rejected, \
             got {res:?}"
        );
    }

    #[test]
    fn invokespecial_init_owner_match_accepted() {
        // `new D` followed by `invokespecial D.<init>` is well-formed.
        let cp = init_owner_cp();
        let frame = owner_match_frame();
        let mut new_site_classes = std::collections::HashMap::new();
        new_site_classes.insert(0u16, std::sync::Arc::<str>::from("D")); // new D == owner

        let res = check_new_init_owner_match(
            &Instruction::Invokespecial(6),
            &frame,
            &cp,
            &new_site_classes,
            "Test",
            "test",
        );
        assert!(
            res.is_ok(),
            "invokespecial D.<init> on an object created by `new D` must be accepted, \
             got {res:?}"
        );
    }

    #[test]
    fn invokespecial_init_unknown_new_site_is_skipped() {
        // If the `new` site was never recorded (e.g. the uninitialized value
        // arrived via a declared StackMapTable frame), we cannot compare —
        // the check must be a no-op rather than a false rejection.
        let cp = init_owner_cp();
        let frame = owner_match_frame();
        let new_site_classes: std::collections::HashMap<u16, std::sync::Arc<str>> =
            std::collections::HashMap::new(); // offset 0 not present

        let res = check_new_init_owner_match(
            &Instruction::Invokespecial(6),
            &frame,
            &cp,
            &new_site_classes,
            "Test",
            "test",
        );
        assert!(
            res.is_ok(),
            "an unrecorded new-site must skip the owner-match (no false reject), \
             got {res:?}"
        );
    }

    #[test]
    fn invokespecial_init_owner_match_end_to_end_rejected() {
        // End-to-end through the pre-Java-7 linear walk: a Java-5 method that
        // does `new C; dup; invokespecial D.<init>()V`. The class also carries
        // a jsr method so the per-method routing engages (escape hatch on so
        // the jsr method is tolerated and the owner mismatch is the only cause
        // of failure).
        //
        // Bytecode of the offending method:
        //   0: new #2  (creates D per cp, but we point new at a *different*
        //               class via a dedicated cp below)
        //   3: dup
        //   4: invokespecial #? D.<init>()V
        //   7: return
        //
        // We build a bespoke cp where the `new` index names C and the
        // constructor names D.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,                        // 0
            ConstantPoolEntry::Utf8("C".into()),                 // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2 (new C)
            ConstantPoolEntry::Utf8("D".into()),                 // 3
            ConstantPoolEntry::ClassReference { name_index: 3 }, // 4 (owner D)
            ConstantPoolEntry::Utf8("<init>".into()),            // 5
            ConstantPoolEntry::Utf8("()V".into()),               // 6
            ConstantPoolEntry::NameAndType {
                name_index: 5,
                descriptor_index: 6,
            }, // 7
            ConstantPoolEntry::MethodReference {
                class_index: 4,
                name_and_type_index: 7,
            }, // 8 (D.<init>()V)
        ]);

        // new #2 (C); dup; invokespecial #8 (D.<init>); return
        let code = vec![
            0xbb, 0x00, 0x02, // 0: new C
            0x59, // 3: dup
            0xb7, 0x00, 0x08, // 4: invokespecial D.<init>
            0xb1, // 7: return
        ];

        let mut class = make_pre_java7_jsr_class(
            "subroutine",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x06, // 0: jsr +6 → 6
                0xb1, // 3: return
                0x00, 0x00, // 4..5: pad
                0x4b, // 6: astore_0
                0xa9, 0x00, // 7: ret 0
            ],
            vec![],
        );
        class.constant_pool = cp;
        class.methods.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("makeMismatch"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        });

        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false);
        assert!(
            res.is_err(),
            "new C; invokespecial D.<init> must be rejected by the owner-match, got {res:?}"
        );
    }

    // =======================================================================
    // Map collection without a load decision — the user-defined-loader
    // coverage gap (arch-2026-07-26/access-control-and-map-coverage)
    // =======================================================================

    /// A method whose bytecode the walk cannot get through: `pop` on an empty
    /// operand stack.
    fn underflow_method(name: &str) -> ClassFileMethod {
        plain_method(name, vec![0x57, 0xb1], 1, 1) // pop; return
    }

    /// The property the whole coverage fix rests on: a class whose Pass-3
    /// *verdict* is deferred still gets real maps, and one unverifiable method
    /// does not cost its siblings theirs.
    #[test]
    fn collect_class_type_maps_never_rejects_and_keeps_good_methods() {
        let class = make_pre_java7_class_with_id(
            91_001,
            vec![
                make_method("abs", "()V", MethodAccessFlags::ABSTRACT, false),
                underflow_method("cannotWalk"),
                // aconst_null; astore_0; return
                plain_method("fine", vec![0x01, 0x4b, 0xb1], 1, 1),
                double_jsr_method("legacyFinally"),
            ],
        );

        // Control: the *verifying* entry point rejects this class outright, so
        // before the fix a deferred-Pass-3 class published nothing at all.
        assert!(
            verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false).is_err(),
            "fixture must be one the verifying path rejects"
        );

        let maps = collect_class_type_maps(&class, &PermissiveHierarchy);
        assert!(maps.verified(), "collection is not a `skipped` marker");
        assert_eq!(maps.method_count(), 4, "one entry per Class::methods slot");

        assert!(maps.method(0).is_none(), "abstract method has no body");

        let bad = maps
            .method(1)
            .expect("unwalkable method still gets an entry");
        assert_eq!(bad.entry_count(), 0, "no rows may be invented for it");
        assert_eq!(
            bad.fast_path_veto(),
            Some(FastPathVeto::IncompleteWalk),
            "the reason must be recorded, not merely the absence of rows"
        );
        assert!(!bad.safe_for_fast_path());

        let good = maps.method(2).expect("the sibling keeps its maps");
        assert!(
            good.entry_count() > 0,
            "a walkable method must not be punished for its sibling"
        );
        assert!(
            good.local_oops_at(2).expect("row at pc 2").get(0),
            "local 0 holds the stored reference from pc 2 onward"
        );
        assert!(good.safe_for_fast_path());

        assert_eq!(
            maps.method(3).and_then(|m| m.fast_path_veto()),
            Some(FastPathVeto::Subroutine),
            "the jsr/ret method keeps its own, more informative veto"
        );
    }

    /// `publish_deferred_class_type_maps` must make the class visible to
    /// `type_maps_for` / `verification_status` — the deferred path's whole
    /// point. Before the fix these answered `Unknown` for every
    /// user-defined-loader class in the process.
    #[test]
    fn deferred_publish_makes_maps_visible() {
        let class = make_pre_java7_class_with_id(
            91_002,
            vec![plain_method("fine", vec![0x01, 0x4b, 0xb1], 1, 1)],
        );
        assert_eq!(
            crate::type_maps::verification_status(class.id),
            crate::type_maps::VerificationStatus::Unknown,
            "nothing published yet"
        );
        assert!(publish_deferred_class_type_maps(
            &class,
            &PermissiveHierarchy
        ));
        assert_eq!(
            crate::type_maps::verification_status(class.id),
            crate::type_maps::VerificationStatus::Verified
        );
        assert!(crate::type_maps::type_maps_for(class.id, 0)
            .is_some_and(|m| m.entry_count() > 0 && m.safe_for_fast_path()));
    }

    /// REDEFINE: `publish_class_type_maps` is first-writer-wins, so after a
    /// class's bytecode is replaced the old maps would keep describing the old
    /// bodies unless something *replaces* them. A stale oop map is a wrong oop
    /// map. `refresh_class_type_maps` is what closes that.
    #[test]
    fn refresh_replaces_stale_maps_after_a_redefine() {
        // v1: one 3-instruction method.
        let mut class = make_pre_java7_class_with_id(
            91_003,
            vec![plain_method("m", vec![0x01, 0x4b, 0xb1], 1, 1)],
        );
        assert!(publish_deferred_class_type_maps(
            &class,
            &PermissiveHierarchy
        ));
        let v1_rows = crate::type_maps::type_maps_for(class.id, 0)
            .expect("v1 maps")
            .entry_count();
        assert_eq!(v1_rows, 3);

        // v2: the redefined body is longer. A no-replace publish is a no-op...
        class.methods = vec![plain_method("m", vec![0x01, 0x4b, 0x01, 0x4b, 0xb1], 1, 1)];
        assert!(
            !crate::type_maps::publish_class_type_maps(
                class.id,
                collect_class_type_maps(&class, &PermissiveHierarchy)
            ),
            "first-writer-wins: this is exactly why redefine needs `replace`"
        );
        assert_eq!(
            crate::type_maps::type_maps_for(class.id, 0)
                .expect("maps")
                .entry_count(),
            v1_rows,
            "the stale v1 rows are still what a consumer would read"
        );

        // ...and `refresh` is what actually installs the new bodies' maps.
        assert!(refresh_class_type_maps(&class, &PermissiveHierarchy));
        assert_eq!(
            crate::type_maps::type_maps_for(class.id, 0)
                .expect("v2 maps")
                .entry_count(),
            5,
            "the maps must describe the bytecode that is actually installed"
        );
    }

    /// `refresh_class_type_maps` must also overwrite a
    /// `mark_class_verification_skipped` marker — a class defined with
    /// `skip_verification` and later redefined into ordinary bytecode should
    /// stop answering `Skipped`.
    #[test]
    fn refresh_overwrites_a_verification_skipped_marker() {
        let class = make_pre_java7_class_with_id(
            91_004,
            vec![plain_method("m", vec![0x01, 0x4b, 0xb1], 1, 1)],
        );
        assert!(crate::type_maps::mark_class_verification_skipped(class.id));
        assert_eq!(
            crate::type_maps::verification_status(class.id),
            crate::type_maps::VerificationStatus::Skipped
        );
        assert!(refresh_class_type_maps(&class, &PermissiveHierarchy));
        assert_eq!(
            crate::type_maps::verification_status(class.id),
            crate::type_maps::VerificationStatus::Verified
        );
    }

    // =====================================================================
    // JVMS §4.9.1 structural bytecode verification
    // =====================================================================

    /// A method with a `Code` attribute, a custom descriptor and a custom
    /// exception table — the shapes the structural scan is about.
    fn structural_method(
        name: &str,
        descriptor: &str,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
        exception_table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
    ) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table,
                    attributes: vec![],
                },
            )))],
        }
    }

    fn handler(
        start_pc: u16,
        end_pc: u16,
        handler_pc: u16,
    ) -> cratonvm_reader::attribute::ExceptionTableEntry {
        cratonvm_reader::attribute::ExceptionTableEntry {
            start_pc,
            end_pc,
            handler_pc,
            catch_type: 0, // catch-all (finally)
        }
    }

    /// `iconst_0; istore_0; return` with a valid catch-all handler — the valid
    /// counterpart every rejection test below is measured against.
    fn well_formed_body() -> Vec<u8> {
        vec![0x03, 0x3b, 0xb1]
    }

    #[test]
    fn structural_accepts_a_well_formed_method() {
        let class = make_pre_java7_class_with_id(
            92_001,
            vec![structural_method(
                "m",
                "()V",
                1,
                1,
                well_formed_body(),
                vec![handler(0, 2, 2)],
            )],
        );
        verify_class_structural_bytecode(&class).expect("a well-formed method must pass");
    }

    #[test]
    fn structural_rejects_handler_pc_past_the_code_array() {
        let class = make_pre_java7_class_with_id(
            92_002,
            vec![structural_method(
                "m",
                "()V",
                1,
                1,
                well_formed_body(),
                vec![handler(0, 2, 99)],
            )],
        );
        let err = verify_class_structural_bytecode(&class)
            .expect_err("a handler_pc past the end of the code array must be rejected");
        assert!(err.to_string().contains("handler_pc"), "{err}");
    }

    #[test]
    fn structural_rejects_handler_pc_inside_an_instruction() {
        // `sipush` is 3 bytes (0x11 hi lo); handler_pc = 1 lands on its operand.
        let class = make_pre_java7_class_with_id(
            92_003,
            vec![structural_method(
                "m",
                "()V",
                1,
                1,
                vec![0x11, 0x00, 0x01, 0x57, 0xb1], // sipush 1; pop; return
                vec![handler(0, 3, 1)],
            )],
        );
        let err = verify_class_structural_bytecode(&class)
            .expect_err("a handler entry inside an instruction must be rejected");
        assert!(err.to_string().contains("instruction boundary"), "{err}");
    }

    #[test]
    fn structural_rejects_inverted_handler_range() {
        let class = make_pre_java7_class_with_id(
            92_004,
            vec![structural_method(
                "m",
                "()V",
                1,
                1,
                well_formed_body(),
                vec![handler(2, 0, 2)],
            )],
        );
        assert!(verify_class_structural_bytecode(&class).is_err());
    }

    #[test]
    fn structural_rejects_empty_handler_range() {
        let class = make_pre_java7_class_with_id(
            92_005,
            vec![structural_method(
                "m",
                "()V",
                1,
                1,
                well_formed_body(),
                vec![handler(1, 1, 2)],
            )],
        );
        assert!(verify_class_structural_bytecode(&class).is_err());
    }

    // ---------------------------------------------------------------------
    // JVMS §4.7.3 `catch_type`, enforced by `bytecode_verifier::catch_type_of`
    //
    // Not part of the structural scan: the rule is a constant-pool
    // cross-check, so it runs on the type-state pass rather than the
    // hierarchy-independent one. `cratonvm_reader` enforces the same rule
    // while it decodes the `Code` attribute and therefore rejects malformed
    // *bytes* first — which is why the corpus case for this shape
    // (`handler_with_a_non_class_catch_type_rejected`) asserts at the reader.
    // These are the verifier's own half, reached the way any in-memory
    // `CodeAttribute` producer reaches it — synthetic stubs, cached class data,
    // tests — where no reader ever saw the bytes.
    // ---------------------------------------------------------------------

    /// `sipush 1; pop; return; astore_1; return` — a region guarded over
    /// `0..4` whose handler entry at 5 is reached only along the exception
    /// edge. Instruction boundaries at 0, 3, 4, 5, 6.
    fn catch_type_body() -> Vec<u8> {
        vec![0x11, 0x00, 0x01, 0x57, 0xb1, 0x4c, 0xb1]
    }

    /// [`catch_type_body`] guarded by a single handler with the given
    /// `catch_type`, over a caller-supplied constant pool.
    fn catch_type_class(raw_id: u32, cp: ConstantPool, catch_type: u16) -> Class {
        let mut class = make_pre_java7_class_with_id(
            raw_id,
            vec![structural_method(
                "m",
                "()V",
                1,
                2,
                catch_type_body(),
                vec![cratonvm_reader::attribute::ExceptionTableEntry {
                    start_pc: 0,
                    end_pc: 4,
                    handler_pc: 5,
                    catch_type,
                }],
            )],
        );
        class.constant_pool = cp;
        class
    }

    #[test]
    fn rejects_a_handler_whose_catch_type_is_not_a_class() {
        // Entry #1 is a Utf8, not a CONSTANT_Class.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,                  // 0
            ConstantPoolEntry::Utf8("not a class".into()), // 1
        ]);
        let class = catch_type_class(92_101, cp, 1);
        let err = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false)
            .expect_err("a catch_type that is not a CONSTANT_Class must be rejected");
        assert!(err.to_string().contains("CONSTANT_Class"), "{err}");
    }

    #[test]
    fn rejects_a_handler_whose_catch_type_is_out_of_range() {
        // Nothing at index 7 at all — the pre-fix fallback silently typed the
        // handler entry as `java/lang/Throwable` and verified clean.
        let class = catch_type_class(92_102, empty_cp(), 7);
        let err = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false)
            .expect_err("an out-of-range catch_type must be rejected");
        assert!(err.to_string().contains("CONSTANT_Class"), "{err}");
    }

    /// The valid counterparts: without these, the two rejections above are
    /// satisfied by a verifier that refuses every handler.
    #[test]
    fn accepts_a_handler_whose_catch_type_is_a_class() {
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,                          // 0
            ConstantPoolEntry::Utf8("java/lang/Exception".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 },   // 2
        ]);
        let class = catch_type_class(92_103, cp, 2);
        verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false)
            .expect("a CONSTANT_Class catch_type must verify");
    }

    #[test]
    fn accepts_a_catch_all_handler() {
        // `catch_type == 0` is the `finally` form and names no pool entry.
        let class = catch_type_class(92_104, empty_cp(), 0);
        verify_class_bytecode_inner(&class, &PermissiveHierarchy, true, false)
            .expect("a catch-all handler must verify");
    }

    #[test]
    fn structural_rejects_local_index_past_max_locals() {
        // `istore_3` with max_locals = 1.
        let class = make_pre_java7_class_with_id(
            92_006,
            vec![structural_method(
                "m",
                "()V",
                1,
                1,
                vec![0x03, 0x3e, 0xb1], // iconst_0; istore_3; return
                vec![],
            )],
        );
        let err = verify_class_structural_bytecode(&class)
            .expect_err("a local operand past max_locals must be rejected");
        assert!(err.to_string().contains("max_locals"), "{err}");
    }

    #[test]
    fn structural_rejects_cat2_local_straddling_max_locals() {
        // `lstore_1` needs slots 1 and 2, but max_locals = 2 stops at slot 1.
        let class = make_pre_java7_class_with_id(
            92_007,
            vec![structural_method(
                "m",
                "()V",
                2,
                2,
                vec![0x09, 0x40, 0xb1], // lconst_0; lstore_1; return
                vec![],
            )],
        );
        assert!(verify_class_structural_bytecode(&class).is_err());
        // The same body with one more local slot is fine.
        let ok = make_pre_java7_class_with_id(
            92_008,
            vec![structural_method(
                "m",
                "()V",
                2,
                3,
                vec![0x09, 0x40, 0xb1],
                vec![],
            )],
        );
        verify_class_structural_bytecode(&ok).expect("lstore_1 fits when max_locals is 3");
    }

    #[test]
    fn structural_rejects_max_locals_smaller_than_the_argument_slots() {
        // `static m(J)V` needs 2 local slots for its single `long` argument.
        let class = make_pre_java7_class_with_id(
            92_009,
            vec![structural_method("m", "(J)V", 1, 1, vec![0xb1], vec![])],
        );
        let err = verify_class_structural_bytecode(&class)
            .expect_err("max_locals must cover the method's own arguments");
        assert!(err.to_string().contains("max_locals"), "{err}");

        let ok = make_pre_java7_class_with_id(
            92_010,
            vec![structural_method("m", "(J)V", 1, 2, vec![0xb1], vec![])],
        );
        verify_class_structural_bytecode(&ok).expect("max_locals = 2 covers a long argument");
    }

    #[test]
    fn structural_rejects_out_of_range_branch_target() {
        // `goto +100` in a 4-byte method.
        let class = make_pre_java7_class_with_id(
            92_011,
            vec![structural_method(
                "m",
                "()V",
                1,
                1,
                vec![0xa7, 0x00, 0x64, 0xb1],
                vec![],
            )],
        );
        assert!(verify_class_structural_bytecode(&class).is_err());
    }

    #[test]
    fn structural_rejects_branch_into_the_middle_of_an_instruction() {
        // `goto +2` lands on the second byte of the following `sipush`.
        let class = make_pre_java7_class_with_id(
            92_012,
            vec![structural_method(
                "m",
                "()V",
                1,
                1,
                vec![
                    0xa7, 0x00, 0x02, // 0: goto 2   (mid-`goto` operand)
                    0x11, 0x00, 0x01, // 3: sipush 1
                    0x57, // 6: pop
                    0xb1, // 7: return
                ],
                vec![],
            )],
        );
        assert!(verify_class_structural_bytecode(&class).is_err());
    }

    #[test]
    fn structural_rejects_empty_code_array() {
        let class = make_pre_java7_class_with_id(
            92_013,
            vec![structural_method("m", "()V", 1, 1, vec![], vec![])],
        );
        assert!(verify_class_structural_bytecode(&class).is_err());
    }

    #[test]
    fn structural_skips_abstract_and_native_methods() {
        // They have no `Code`, so there is nothing to scan and nothing to
        // reject — the presence rules live in Pass 2.
        let mut class = make_pre_java7_class_with_id(92_014, vec![]);
        class.methods = vec![
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
                name: Arc::from("a"),
                descriptor: Arc::from("()V"),
                attributes: vec![],
            },
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: Arc::from("n"),
                descriptor: Arc::from("(J)V"),
                attributes: vec![],
            },
        ];
        verify_class_structural_bytecode(&class).expect("no Code, nothing to verify");
    }
}
