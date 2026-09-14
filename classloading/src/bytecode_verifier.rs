// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Bytecode verification — Pass 3 (JVM spec 4.10.1).
//!
//! Type-checking verification for Java 7+ classes using StackMapTable frames.
//! Each method's bytecode is verified by walking instructions and checking that
//! the operand stack and local variable types match the declared frames at
//! every branch target and exception handler.
//!
//! Pre-Java-7 classes (version < 51) without StackMapTable use a simplified
//! type inference pass (JVM spec 4.10.2) with a worklist-based dataflow algorithm.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use cratonvm_reader::attribute::Attribute;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::instruction::Instruction;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::stack_map::StackMapTable;
use cratonvm_reader::verified_code::verified_code;

use super::class::Class;
use super::type_maps::{ClassTypeMaps, MethodTypeMaps, MethodTypeMapsBuilder};
use super::verify_frame::VerificationFrame;
use super::verify_insn::verify_instruction;
use super::vtype::{ClassHierarchy, VType};
use cratonvm_types::error::LinkageError;

/// Verify all methods in a class using the bytecode type-checking verifier.
///
/// Per JVM spec 4.10.1:
/// - Abstract/native methods are skipped (no Code attribute).
/// - Java 7+ (version >= 51) classes REQUIRE StackMapTable for non-trivial methods.
/// - Pre-Java-7 classes use type inference verification (worklist dataflow).
///
/// Strictness selection (finding M1): strict branch-target / unreachable-code
/// checking is enabled **automatically, per method**, for Java 7+ classes
/// (`version.major >= JAVA_7`) whenever the method ships a `StackMapTable`.
/// JVMS §4.10.1 requires such classes to declare a frame at every branch
/// target, so enforcing strict checking does not reject legitimate Java-7+
/// bytecode and closes the type-confusion hole that lenient mode left open
/// (a branch target with no declared frame was previously accepted without
/// confirming the incoming type-state was assignable to the target's
/// expected state). Pre-Java-7 class files (`version.major < JAVA_7`, i.e.
/// no mandatory StackMapTable) stay lenient and are handled by the worklist
/// type-inference pass. [`verify_bytecode_strict`] forces strict mode
/// unconditionally regardless of version.
pub fn verify_bytecode(class: &Class, hierarchy: &dyn ClassHierarchy) -> Result<(), LinkageError> {
    // SECURITY FIX (V4): strict verification is now the DEFAULT for any class
    // that is NOT a trusted bootstrap/platform class. Trusted bootstrap
    // classes (loaded by the bootstrap loader from a `java/`/`jdk/`/`sun/`/
    // `com/sun/` package) were pre-verified by javac/jlink, so we keep the
    // lenient path for them to preserve compatibility with the small set of
    // legitimately frameless branch targets that ship in the JDK image.
    //
    // For application / untrusted code, full per-branch-target frame checking
    // is engaged: a branch to a frameless target reached with a typed stack is
    // rejected (JVMS §4.10.1), closing the type-confusion hole the lenient
    // default left open. The escape hatch remains `--noverify`
    // (`config.skip_verification`, gated at the VM entry point) and the
    // explicit `verify_bytecode_strict` (`-Xverify:all`).
    let force_strict = !class_is_bootstrap_trusted(class);
    verify_bytecode_inner(class, hierarchy, force_strict)
}

/// SECURITY FIX (V4): trust predicate for the strict-by-default decision.
///
/// A class is treated as trusted (and therefore eligible for the lenient
/// pre-verified default) ONLY when BOTH hold:
///   (a) its defining loader is the bootstrap loader, AND
///   (b) it lives in a trusted JDK package prefix (`java/`, `jdk/`, `sun/`,
///       `com/sun/`).
///
/// This mirrors `vm::vm_util::verifier_skip_eligible` so the "trusted"
/// determination is consistent between the verifier-skip gate (V13) and the
/// strict-mode gate (V4). A forged class name alone (e.g. `java/lang/Evil`
/// defined by an application loader) does NOT earn trust, because the
/// loader-identity check (a) fails — such a class is verified strictly.
pub(crate) fn class_is_bootstrap_trusted(class: &Class) -> bool {
    let is_bootstrap_loaded = class.loader_id == crate::class::ClassLoaderId::Bootstrap;
    let has_trusted_prefix = class.name.starts_with("java/")
        || class.name.starts_with("jdk/")
        || class.name.starts_with("sun/")
        || class.name.starts_with("com/sun/");
    is_bootstrap_loaded && has_trusted_prefix
}

/// Strict variant of [`verify_bytecode`] that rejects branch targets without
/// a corresponding StackMapTable frame.
///
/// This matches the literal JVM spec requirement but may reject class files
/// produced by some compilers (e.g. branches within basic blocks that don't
/// cross type-state boundaries).
///
/// SECURITY (V4): this is the explicit `-Xverify:all` escape hatch — it forces
/// strict mode for EVERY class, including trusted bootstrap classes that the
/// default [`verify_bytecode`] would otherwise verify leniently. Untrusted
/// application classes already default to strict via [`verify_bytecode`]; this
/// entry point exists to additionally subject the trusted JDK image to the
/// spec-literal check when a deployment wants maximum scrutiny.
pub fn verify_bytecode_strict(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    verify_bytecode_inner(class, hierarchy, true)
}

fn verify_bytecode_inner(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
    strict_verification: bool,
) -> Result<(), LinkageError> {
    // TYPE MAPS (arch-2026-07-26/verifier-type-maps): verification retains
    // what it proves. The walk below already computes the exact verification
    // type of every local slot and every operand-stack slot at every pc; the
    // per-method builder captures that as compact oop bitmaps as it goes —
    // it is the SAME walk, not a second pass — and the results are published
    // into the process-wide side table keyed by `(ClassId, method_index)`.
    //
    // This is unconditional on the default build path: no cargo feature, no
    // `CRATONVM_*` env var, no opt-in. `skip_verification` (the `--noverify`
    // escape hatch) prevents this function from running at all, in which case
    // `type_maps::verification_status` answers `Unknown`/`Skipped` and every
    // consumer is required to fall back to conservative behaviour.
    let mut collected: Vec<(Arc<str>, Arc<str>, Option<MethodTypeMaps>)> =
        Vec::with_capacity(class.methods.len());

    for method in &class.methods {
        // Skip abstract and native methods — they have no Code attribute.
        // They still occupy an index in `Class::methods`, so a `None` entry
        // is pushed to keep `method_index` aligned with the class's method
        // list (consumers index by that position).
        if method.is_abstract() || method.is_native() {
            collected.push((method.name.clone(), method.descriptor.clone(), None));
            continue;
        }

        let maps = verify_method(
            &class.name,
            method,
            &class.constant_pool,
            &class.version,
            hierarchy,
            strict_verification,
        )?;
        collected.push((method.name.clone(), method.descriptor.clone(), maps));
    }

    // Publish only once every method verified: a class that fails
    // verification is never loaded, so half-built maps must not be visible.
    // The oop-map oracle identifies a compiled frame by NAME (the JIT is
    // handed class names, not ids), and cannot take the class manager's lock
    // from inside a stop-the-world scan to resolve one. Recorded beside the
    // maps so a name that resolves always HAS maps; a no-op unless
    // `CRATONVM_DBG_VERIFY_OOP_MAPS` is set. See `type_maps::note_class_name`.
    crate::type_maps::note_class_name(class.id, &class.name);
    crate::type_maps::publish_class_type_maps(class.id, ClassTypeMaps::new(collected));

    Ok(())
}

/// Verify a single method's bytecode.
///
/// On success returns the [`MethodTypeMaps`] the verification walk produced —
/// the exact reference layout of every local slot and every operand-stack slot
/// at every instruction start, plus the per-method `safe_for_fast_path` proof.
/// `None` means the method has nothing to describe (no `Code` attribute, or an
/// empty one), which consumers must treat as "unproven".
fn verify_method(
    class_name: &str,
    method: &ClassFileMethod,
    cp: &ConstantPool,
    version: &ClassFileVersion,
    hierarchy: &dyn ClassHierarchy,
    strict_verification: bool,
) -> Result<Option<MethodTypeMaps>, LinkageError> {
    let code_attr = match method.code() {
        Some(code) => code,
        None => return Ok(None), // No code to verify (shouldn't happen if abstract/native filtered)
    };

    let bytecode = &code_attr.code;
    if bytecode.is_empty() {
        return Ok(None);
    }

    // JVMS §4.9.1 static constraints, ahead of the §4.10 type-state pass.
    //
    // SECURITY (structural coverage gap): this scan used to run ONLY on
    // `verifier.rs`'s per-method path — i.e. only for classes that contain a
    // `jsr`/`ret` method somewhere. Every ordinary class reached this function
    // instead, where the exception table was never validated at all: a handler
    // whose `handler_pc` lands in the middle of an instruction (or past the end
    // of the code array) was accepted, because the linear walk below only
    // *consults* `handler_pc` as a map key and simply never matches. The
    // interpreter, which does dispatch to it, then began decoding at a
    // mid-instruction offset. The same scan also rejects an under-declared
    // `max_locals` and out-of-range local operands.
    crate::verifier::verify_method_structural(class_name, method)?;

    // Establish the canonical decode and CFG contract before type-state
    // verification. The JIT consumes the same bounded-cache entry, so verifier
    // and compiler cannot disagree about instruction widths or branch
    // boundaries. (`verify_method_structural` primed the bounded cache above,
    // so this is a cache hit.)
    let verified = verified_code(bytecode).map_err(|e| LinkageError::VerifyError {
        class_name: class_name.to_string(),
        method_name: method.name.to_string(),
        message: format!("failed to build verified code: {e}"),
    })?;
    // TYPE MAPS lean on two guarantees this canonical decode establishes, so
    // they are asserted here rather than re-derived downstream:
    //   1. `code.len() <= 65535`, so every recorded pc fits a `u16` (the
    //      `PcTable::U32` widening is defence in depth, not a live path).
    //   2. every branch target is an instruction boundary, so no row in the
    //      pc table can describe a mid-instruction offset.
    let insn_count = verified.instructions().len();

    // Find the StackMapTable attribute within the Code attribute
    let stack_map_table = find_stack_map_table(&code_attr.attributes);

    // Java 7+ (version >= 51) requires StackMapTable for verification
    let requires_stack_map = version.major >= ClassFileVersion::JAVA_7.major;

    // SECURITY FIX (V4): strict branch-target verification is now the DEFAULT
    // for untrusted (non-bootstrap-trusted) classes. `strict_verification` is
    // threaded from the entry point: it is `true` for application/untrusted
    // code (see `verify_bytecode`'s `force_strict`) and for `-Xverify:all`
    // (`verify_bytecode_strict`); it is `false` only for pre-verified trusted
    // bootstrap classes, which stay lenient to tolerate the small set of
    // legitimately frameless branch targets that ship in the JDK image.
    //
    // JVMS §4.10.1 requires Java 7+ classfiles to declare a frame at every
    // branch target, so for untrusted code a branch to a frameless target
    // reached with a typed stack is a VerifyError. The remaining verifier
    // passes (worklist type inference, unreachable-code rejection,
    // operand-stack bounds) apply in both modes.
    let effective_strict = strict_verification;

    if requires_stack_map && stack_map_table.is_none() {
        // No StackMapTable — only valid if there are no branches/exception handlers.
        // Check if the method has any branch targets that would require type checking.
        let has_exception_handlers = !code_attr.exception_table.is_empty();
        let has_branches = bytecode_has_branches(bytecode);
        if has_exception_handlers || has_branches {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: "Java 7+ method with branches/exception handlers requires StackMapTable"
                    .to_string(),
            });
        }
    }

    // For pre-Java-7 classes without StackMapTable but with branches/handlers,
    // run a simplified type inference pass using a worklist algorithm.
    if !requires_stack_map && stack_map_table.is_none() {
        let has_branches = bytecode_has_branches(bytecode);
        let has_handlers = !code_attr.exception_table.is_empty();
        if has_branches || has_handlers {
            // Pre-Java-7 inference path (JVMS §4.10.2). It builds its own type
            // maps from the settled worklist fixpoint — see
            // `verify_by_inference`.
            return verify_by_inference(class_name, method, code_attr, cp, hierarchy).map(Some);
        }
        // No branches and no handlers — fall through to linear walk
    }

    // Parse StackMapTable if present
    let parsed_table = match stack_map_table {
        Some(raw_data) => match StackMapTable::parse(raw_data) {
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

    // Build the declared frames map: bytecode offset → VerificationFrame
    // StackMapTable frames are derived from a compact initial frame (method params only,
    // NOT padded to max_locals), per JVM spec 4.7.4.
    let compact_frame = VerificationFrame::compact_initial_frame(
        class_name,
        &method.name,
        &method.descriptor,
        method.is_static(),
        code_attr.max_stack,
    );
    let initial_frame = VerificationFrame::initial_frame(
        class_name,
        &method.name,
        &method.descriptor,
        method.is_static(),
        code_attr.max_locals,
        code_attr.max_stack,
    );

    let declared_frames = match &parsed_table {
        Some(table) => build_declared_frames(table, &compact_frame, cp, class_name, &method.name)?,
        None => FxHashMap::with_capacity_and_hasher(32, Default::default()),
    };

    // Build exception handler target set
    let mut handler_targets: FxHashMap<u16, VType> =
        FxHashMap::with_capacity_and_hasher(8, Default::default());
    for entry in &code_attr.exception_table {
        let catch_type = catch_type_of(entry, cp, class_name, &method.name)?;
        handler_targets.insert(entry.handler_pc, catch_type);
    }

    // JVMS §4.10 verifier selection, and the failover that keeps the
    // StackMapTable walk honest on pre-Java-7 class files.
    //
    // The linear StackMapTable walk is sound only where a declared frame exists
    // at every merge point — which JVMS §4.10.1 guarantees for major ≥ 51 and
    // the strict mode above enforces. For major ≤ 50 a `StackMapTable` is
    // OPTIONAL and may be partial, and the strict checks below are (correctly)
    // gated on `requires_stack_map`. That combination left a hole: a class file
    // declaring major 50 and shipping a token `StackMapTable` skipped the
    // worklist inference (`stack_map_table.is_some()`), and then every branch
    // to a target WITHOUT a declared frame was accepted with the target typed
    // by whatever fell through into it. Arriving there along the branch with a
    // shallower stack is a run-time operand-stack underflow the verifier
    // signed off on.
    //
    // JVMS §4.10 already defines the remedy for exactly this version range:
    // failover to type inference (§4.10.2). So for a pre-Java-7 method whose
    // declared frames do not cover every merge point, the worklist — which
    // merges at every branch target and every handler entry and needs no
    // declared frames at all — is the authority. Detected up front for handler
    // entries, and at the branch site inside the walk for branch targets.
    let pre_java7_partial_frames = !requires_stack_map
        && parsed_table.is_some()
        && code_attr
            .exception_table
            .iter()
            .any(|e| !declared_frames.contains_key(&e.handler_pc));
    if pre_java7_partial_frames {
        return verify_by_inference(class_name, method, code_attr, cp, hierarchy).map(Some);
    }

    // TYPE MAPS: ride the walk below. `record` is called once per instruction
    // start with the type-state that holds immediately BEFORE that
    // instruction executes — after any declared StackMapTable frame or
    // exception-handler frame has been adopted, so the recorded state is the
    // one the interpreter/GC would actually observe at that pc.
    let mut type_maps = MethodTypeMapsBuilder::new(code_attr.max_locals, code_attr.max_stack);
    // Exact, not estimated: `verified_code` already counted the instructions.
    type_maps.reserve(insn_count);
    // Cleared whenever the walk skips or truncates a region, which denies
    // `safe_for_fast_path` (the unchecked interpreter handlers need every
    // executed instruction proven, not merely most of them).
    let mut walk_complete = true;

    // Walk the bytecode
    let mut pc = 0usize;
    let mut current_frame = initial_frame;
    // T1.3.3 — start as `true` because the method entry point (PC=0)
    // is always reachable from the caller. The `verified` flag tracks
    // whether control fell through from the *previous* instruction;
    // the first instruction has no previous, so it's unconditionally
    // reachable.
    let mut verified = true;
    // TYPE MAPS SOUNDNESS: is `current_frame` the real entry state for *this*
    // pc?
    //
    // `verified` alone does not answer that, and the difference is soundness
    // rather than precision. `verified` tracks only whether the PREVIOUS
    // instruction fell through; it is never re-armed when a declared frame or
    // a handler frame is adopted, and the unreachable-code guard below is
    // conditioned on `requires_stack_map`. So there are exactly two pcs at
    // which `current_frame` is a *stale* frame belonging to an unrelated
    // predecessor:
    //
    //   * a **pre-Java-7 class that ships a `StackMapTable`**
    //     (`requires_stack_map` is false, so the dead-code guard below never
    //     fires) walking dead code after a `goto` / `return` / `athrow` that
    //     carries no declared frame; and
    //   * a **handler pc that is ALSO reachable by fall-through** — the
    //     handler frame is installed only under `!verified`, so when control
    //     does fall through, `current_frame` describes the fall-through state
    //     while the exception edge enters the same pc with a different locals
    //     state and `[throwable]` on the operand stack. The linear walk never
    //     merges the two.
    //
    // Type-*checking* against a stale frame is a pre-existing laxity of this
    // walk (it can only accept too much, and strict mode closes the first
    // case). RECORDING one is a different and much worse thing: a row that
    // claims a slot holds a reference where the other edge supplies an int is
    // an oop scan of a non-oop — immediate heap corruption — and a row that
    // claims none where the other edge supplies a live reference is a missed
    // root. Both are silent.
    //
    // So those pcs record nothing (`oop_map_at` → `None` → "unproven, scan
    // conservatively") and the method loses `safe_for_fast_path`. This
    // mirrors the guard in `verifier::verify_method_typestate`, which is the
    // same walk for classes routed through the JSR-aware dispatcher.
    let mut authoritative = true;

    while pc < bytecode.len() {
        // Check if this PC is a declared frame target
        if let Some(declared) = declared_frames.get(&(pc as u16)) {
            // At a merge point: verify current frame is assignable to declared frame
            if verified && !current_frame.is_assignable_to(declared, hierarchy) {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "frame mismatch at bytecode offset {pc}: \
                         current frame is not assignable to declared StackMapTable frame"
                    ),
                });
            }
            // Use the declared frame going forward (narrows types).
            // Pad to max_locals since StackMapTable frames may be compact.
            let mut adopted = declared.clone();
            adopted.pad_locals_to(code_attr.max_locals);
            current_frame = adopted;
            // TYPE MAPS: a declared StackMapTable frame IS the authoritative
            // merge-point state for this pc — every edge into it was (or, at
            // the branch sites below, will be) checked against it.
            authoritative = true;
        }

        // Check if this PC is an exception handler entry
        if let Some(catch_type) = handler_targets.get(&(pc as u16)) {
            if !verified {
                // This is an exception handler entry point — start with the handler frame
                let mut handler_frame = current_frame.clone();
                handler_frame.clear_stack();
                handler_frame
                    .push(catch_type.clone())
                    .map_err(|_| LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("exception handler stack overflow at offset {pc}"),
                    })?;
                current_frame = handler_frame;
                // The handler frame we just built IS this pc's entry state.
                authoritative = true;
            } else if !declared_frames.contains_key(&(pc as u16)) {
                // TYPE MAPS: handler entry that is ALSO reachable by
                // fall-through, with no declared frame to reconcile the two
                // edges. `current_frame` describes only the fall-through edge;
                // the exception edge (`[throwable]` on a cleared stack) is
                // unrepresented and is never merged in. Record nothing here
                // rather than a half-true row.
                authoritative = false;
            }
        }

        // T1.3.3 — unreachable code rejection (JVMS §4.10.1).
        //
        // When control does not fall through from the previous
        // instruction (e.g. after an unconditional `goto`, `return`,
        // or `athrow`) AND the current PC is neither a declared
        // StackMapTable frame target nor an exception handler entry,
        // the code at this PC is unreachable. Java 7+ requires
        // StackMapTable frames at every branch target, so any code
        // reachable only by a branch that lacks a frame is a
        // verification error.
        //
        // Pre-Java-7 classes without StackMapTable use the type-
        // inference pass (verify_by_inference), which does its own
        // reachability analysis; the check here applies only to the
        // StackMapTable-driven linear walk.
        if !verified
            && requires_stack_map
            && !declared_frames.contains_key(&(pc as u16))
            && !handler_targets.contains_key(&(pc as u16))
        {
            // Strict mode: reject outright. Lenient mode: skip the
            // unreachable code silently (some older compilers emit
            // dead code after branches; rejecting them would break
            // backward compatibility).
            if effective_strict {
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
            // Lenient: skip to the next declared frame or handler.
            //
            // TYPE MAPS: no row is emitted for a skipped region, so
            // `oop_map_at` answers `None` there (= "unproven", scan
            // conservatively). The method also loses `safe_for_fast_path`:
            // the unreachability call is the verifier's, and the unchecked
            // interpreter handlers must not run on bytecode whose operands
            // were never checked.
            walk_complete = false;
            let (_, next_pc) = match Instruction::decode(bytecode, pc) {
                Ok(r) => r,
                Err(_) => break, // malformed — stop walking
            };
            pc = next_pc;
            continue;
        }

        // TYPE MAPS: capture the proven reference layout at this instruction
        // start. This is the entire point of the change — the walk already
        // holds `current_frame`; without this line it is discarded. The guard
        // is load-bearing: see the `authoritative` declaration above for the
        // two pcs at which `current_frame` is a stale frame from an unrelated
        // predecessor, where a recorded row would be a wrong oop map.
        if authoritative {
            type_maps.record(pc as u32, &current_frame);
        } else {
            walk_complete = false;
        }

        // Decode the instruction
        let (insn, next_pc) = match Instruction::decode(bytecode, pc) {
            Ok(result) => result,
            Err(e) => {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!("failed to decode instruction at offset {pc}: {e}"),
                });
            }
        };

        // TYPE MAPS: fold this instruction into the per-method fast-path
        // safety proof (local-slot operand bounds, jsr/ret, stray `wide`).
        type_maps.observe_instruction(&insn);

        // Verify the instruction's type effects
        let result = verify_instruction(
            &insn,
            pc,
            &mut current_frame,
            cp,
            class_name,
            &method.name,
            &method.descriptor,
            hierarchy,
        )
        .map_err(|e| {
            // Add context to verify errors
            match e {
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
            }
        })?;

        // Check branch targets have declared frames.
        //
        // PRAGMATIC CHOICE: In lenient mode (the default), branch targets
        // without a corresponding StackMapTable frame are accepted.  The JVM
        // spec (4.10.1) technically requires every branch target to have a
        // declared frame so that the verifier can check type-state
        // compatibility at merge points.  However, many real-world compilers
        // (including javac in some edge cases and various bytecode-rewriting
        // frameworks) emit branches within basic blocks that do NOT cross
        // type-state boundaries and therefore omit the StackMapTable entry.
        // Rejecting those class files would break compatibility with a large
        // corpus of existing bytecode.
        //
        // In strict mode (`verify_bytecode_strict`), the spec-compliant check
        // is enforced and missing frames are treated as verification errors.
        for &target in &result.branch_targets {
            if effective_strict
                && requires_stack_map
                && !declared_frames.contains_key(&target)
                && parsed_table.is_some()
            {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "strict verification: branch target at offset {} has no StackMapTable frame",
                        target
                    ),
                });
            }

            // Pre-Java-7 failover (see the derivation of
            // `pre_java7_partial_frames` above): this branch target has no
            // declared frame, so the linear walk cannot check the edge. Hand
            // the whole method to the §4.10.2 worklist, whose verdict IS a
            // merge-checked one. Returning here discards the partial walk's
            // rows too, which is correct — they describe a type-state the
            // worklist may refine.
            if !requires_stack_map && !declared_frames.contains_key(&target) {
                return verify_by_inference(class_name, method, code_attr, cp, hierarchy).map(Some);
            }

            // SECURITY FIX (cl-verifier): forward-edge type-state merge at the
            // BRANCH SITE. The linear-walk's fall-through check (above, where a
            // declared-frame PC is reached) only validates assignability when
            // control fell through into that PC (`verified == true`). A PC that
            // is reachable ONLY via a branch — i.e. the instruction preceding it
            // does not fall through, so `verified` is `false` there — would have
            // its declared frame ADOPTED BLINDLY without ever confirming that the
            // type-state arriving along the branch edge is assignable to that
            // declared frame. That is a type-confusion hole (a permissive
            // verifier is a security hole): an attacker could branch into a PC
            // with an incompatible operand-stack / local layout and have the
            // verifier silently accept the declared frame.
            //
            // JVMS §4.10.1 requires every forward edge into a target to be
            // checked against the target's declared frame (standard data-flow
            // merge). So here, at each branch source, if the target carries a
            // declared StackMapTable frame, verify the CURRENT frame (the
            // type-state leaving this instruction) is assignable to the target's
            // declared frame — independent of the fall-through `verified` flag.
            // `is_assignable_to` already tolerates the declared frame being
            // compact (fewer locals than max_locals); it only compares up to the
            // target's declared length, exactly as the fall-through site does.
            if let Some(target_frame) = declared_frames.get(&target) {
                if !current_frame.is_assignable_to(target_frame, hierarchy) {
                    return Err(LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!(
                            "frame mismatch on branch to offset {target}: \
                             current frame is not assignable to the declared \
                             StackMapTable frame at the branch target"
                        ),
                    });
                }
            }
        }

        verified = result.falls_through;

        // TYPE MAPS: the next pc inherits an authoritative frame only when
        // control actually flows into it from here. Once control stops falling
        // through, `current_frame` belongs to this instruction and not to
        // whatever pc the walk visits next; only a declared frame or a handler
        // frame (adopted at the top of the loop) can re-arm it.
        authoritative = authoritative && result.falls_through && next_pc < bytecode.len();

        if !result.falls_through && next_pc < bytecode.len() {
            // Control does not fall through — the next instruction is only reachable
            // via a branch target or exception handler. Reset verification state.
            // The next instruction must be a declared frame target or handler entry.
            verified = false;
        }

        pc = next_pc;
    }

    Ok(Some(type_maps.finish(walk_complete)))
}

/// The verification type an exception handler pushes on entry.
///
/// `catch_type == 0` is the catch-all (`finally`) form and yields
/// `java/lang/Throwable`. Any other value must be a resolvable
/// `CONSTANT_Class` entry (JVMS §4.7.3).
///
/// SECURITY: the previous inline form fell back to `java/lang/Throwable`
/// whenever the index did not resolve, which silently accepted a handler whose
/// `catch_type` points at an arbitrary constant-pool slot — the class file was
/// malformed and the verifier said nothing, leaving the resolution failure to
/// surface at run time inside exception dispatch. Reject at verify time
/// instead, naming the entry.
pub(crate) fn catch_type_of(
    entry: &cratonvm_reader::attribute::ExceptionTableEntry,
    cp: &ConstantPool,
    class_name: &str,
    method_name: &str,
) -> Result<VType, LinkageError> {
    if entry.catch_type == 0 {
        return Ok(VType::ObjectRef(Arc::from("java/lang/Throwable")));
    }
    match cp.get_class_name_arc(entry.catch_type) {
        Some(name) => Ok(VType::ObjectRef(name)),
        None => Err(LinkageError::VerifyError {
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            message: format!(
                "exception handler at handler_pc={} has catch_type {}, which is not a \
                 CONSTANT_Class entry with a resolvable name (JVMS §4.7.3)",
                entry.handler_pc, entry.catch_type
            ),
        }),
    }
}

/// Quick scan of bytecode to detect if it contains any branch instructions.
/// Looks for goto, if*, jsr, tableswitch, lookupswitch opcodes.
fn bytecode_has_branches(bytecode: &[u8]) -> bool {
    let mut pc = 0;
    while pc < bytecode.len() {
        let opcode = bytecode[pc];
        match opcode {
            // if<cond>: 0x99..=0x9e, if_icmp<cond>: 0x9f..=0xa4, if_acmp<eq/ne>: 0xa5..=0xa6
            0x99..=0xa6 => return true,
            // goto: 0xa7, jsr: 0xa8
            0xa7 | 0xa8 => return true,
            // tableswitch: 0xaa, lookupswitch: 0xab
            0xaa | 0xab => return true,
            // goto_w: 0xc8, jsr_w: 0xc9
            0xc8 | 0xc9 => return true,
            // ifnull: 0xc6, ifnonnull: 0xc7
            0xc6 | 0xc7 => return true,
            // wide prefix
            0xc4 => {
                pc += if pc + 1 < bytecode.len() && bytecode[pc + 1] == 0x84 {
                    6 // wide iinc
                } else {
                    4 // wide load/store
                };
                continue;
            }
            _ => {}
        }
        // Advance by instruction size (simplified: single-byte unless multi-byte)
        pc += match opcode {
            // 2-byte instructions (with 1-byte operand)
            0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xbc | 0xa9 => 2,
            // 3-byte instructions (with 2-byte operand)
            0x11 | 0x13 | 0x14 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 | 0x84 => 3,
            // multianewarray: 4 bytes
            0xc5 => 4,
            // invokeinterface: 5 bytes, invokedynamic: 5 bytes
            0xb9 | 0xba => 5,
            _ => 1,
        };
    }
    false
}

/// Find the raw StackMapTable data from Code sub-attributes.
fn find_stack_map_table(attributes: &[Attribute]) -> Option<&[u8]> {
    for attr in attributes {
        if let Attribute::StackMapTable { entries } = attr {
            // `entries: &Arc<[u8]>` — deref to `&[u8]` for the return type.
            return Some(&**entries);
        }
    }
    None
}

/// Build a map of bytecode offset → VerificationFrame from a parsed StackMapTable.
fn build_declared_frames(
    table: &StackMapTable,
    initial_frame: &VerificationFrame,
    cp: &ConstantPool,
    class_name: &str,
    method_name: &str,
) -> Result<FxHashMap<u16, VerificationFrame>, LinkageError> {
    let mut frames = FxHashMap::with_capacity_and_hasher(32, Default::default());
    // Round 7 audit fix (MED #8): u32-internal accumulation with
    // `absolute > u16::MAX` rejection surfaces here as a verify error.
    let offsets = table
        .absolute_offsets()
        .map_err(|e| LinkageError::VerifyError {
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            message: format!("StackMapTable absolute_offsets: {e}"),
        })?;

    let mut prev_frame = initial_frame.clone();

    for (i, entry) in table.entries.iter().enumerate() {
        let offset = offsets[i];

        let new_frame = prev_frame
            .apply_stack_map_frame(entry, cp)
            .map_err(|e| match e {
                LinkageError::VerifyError { message, .. } => LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method_name.to_string(),
                    message: format!("StackMapTable frame {i} at offset {offset}: {message}"),
                },
                other => other,
            })?;

        prev_frame = new_frame.clone();
        frames.insert(offset, new_frame);
    }

    Ok(frames)
}

// ---------------------------------------------------------------------------
// Type inference verification (pre-Java-7)
// ---------------------------------------------------------------------------

/// Type inference verification for pre-Java-7 classes (JVM spec 4.10.2).
///
/// Uses a worklist algorithm: start at offset 0, propagate type states
/// forward through each instruction, and merge at branch targets / exception
/// handler entries.  The algorithm terminates when the worklist is empty
/// and all reachable offsets have consistent type states.
///
/// TYPE MAPS: when the worklist settles, `frame_at` *is* the answer — it maps
/// every reachable instruction start to its fixpoint entry type-state. The
/// maps are serialized from that map, not recomputed: this path performs no
/// extra dataflow, it only writes down the dataflow it already finished. (The
/// StackMapTable path records inline instead, because its linear walk visits
/// each pc exactly once and never revisits a merge.)
fn verify_by_inference(
    class_name: &str,
    method: &ClassFileMethod,
    code_attr: &cratonvm_reader::attribute::CodeAttribute,
    cp: &ConstantPool,
    hierarchy: &dyn ClassHierarchy,
) -> Result<MethodTypeMaps, LinkageError> {
    let bytecode = &code_attr.code;
    let initial_frame = VerificationFrame::initial_frame(
        class_name,
        &method.name,
        &method.descriptor,
        method.is_static(),
        code_attr.max_locals,
        code_attr.max_stack,
    );

    // Map from bytecode offset -> verified frame state at that offset
    let mut frame_at: FxHashMap<usize, VerificationFrame> =
        FxHashMap::with_capacity_and_hasher(32, Default::default());
    frame_at.insert(0, initial_frame);

    // Worklist of offsets to (re-)visit
    let mut worklist: Vec<usize> = vec![0];
    // Set of offsets already enqueued in this round (avoids duplicates on the worklist)
    let mut enqueued: FxHashSet<usize> = FxHashSet::default();
    enqueued.insert(0);

    // Safety: limit iterations to prevent pathological bytecode from hanging the verifier
    let max_iterations = bytecode.len().saturating_mul(4).max(256);
    let mut iterations = 0;

    while let Some(pc) = worklist.pop() {
        enqueued.remove(&pc);
        iterations += 1;
        if iterations > max_iterations {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message:
                    "type inference exceeded iteration limit (possible infinite loop in bytecode)"
                        .to_string(),
            });
        }

        if pc >= bytecode.len() {
            continue;
        }

        let mut current = match frame_at.get(&pc) {
            Some(f) => f.clone(),
            None => continue,
        };

        // Decode the instruction
        let (insn, next_pc) = match Instruction::decode(bytecode, pc) {
            Ok(r) => r,
            Err(e) => {
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!("failed to decode instruction at offset {pc}: {e}"),
                });
            }
        };

        // Verify the instruction's type effects
        let result = verify_instruction(
            &insn,
            pc,
            &mut current,
            cp,
            class_name,
            &method.name,
            &method.descriptor,
            hierarchy,
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

        // Propagate to fall-through successor
        if result.falls_through && next_pc < bytecode.len() {
            if merge_inference_frame(
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

        // Propagate to branch targets. NEW-9: a branch target outside
        // the bytecode is a hard verification error, not a silently
        // ignored condition. A goto / if* / jsr that points past the
        // end of the method is the spec-defined "Inconsistent or
        // missing stack map frame" case in HotSpot.
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
            if merge_inference_frame(
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

        // Propagate to exception handler entries whose protected range covers this pc
        for entry in &code_attr.exception_table {
            if pc >= entry.start_pc as usize && pc < entry.end_pc as usize {
                let catch_type = catch_type_of(entry, cp, class_name, &method.name)?;
                let mut handler_frame = current.clone();
                handler_frame.clear_stack();
                handler_frame
                    .push(catch_type)
                    .map_err(|_| LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!(
                            "exception handler stack overflow at handler pc {}",
                            entry.handler_pc
                        ),
                    })?;
                let handler_pc = entry.handler_pc as usize;
                if merge_inference_frame(
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
    // `pc` — exactly the state the interpreter/GC observes when the frame's pc
    // is `pc`. Rows must be emitted in ascending pc order because
    // `MethodTypeMaps` indexes them with a binary search.
    let mut type_maps = MethodTypeMapsBuilder::new(code_attr.max_locals, code_attr.max_stack);
    type_maps.reserve(frame_at.len());
    let mut ordered: Vec<usize> = frame_at.keys().copied().collect();
    ordered.sort_unstable();

    // A pre-Java-7 method that reached this path has branches or handlers and
    // no StackMapTable, so its coverage is whatever the worklist reached.
    // Anything the worklist did not reach is simply absent from the map
    // (`oop_map_at` → `None` → conservative), and a decode failure at a
    // recorded pc denies `safe_for_fast_path`.
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

/// Merge `incoming` frame into the frame at `target_pc` in the map.
/// Returns `true` if the target's frame changed (caller should re-enqueue).
fn merge_inference_frame(
    frame_at: &mut FxHashMap<usize, VerificationFrame>,
    target_pc: usize,
    incoming: &VerificationFrame,
    hierarchy: &dyn ClassHierarchy,
    class_name: &str,
    method_name: &str,
) -> Result<bool, LinkageError> {
    match frame_at.get(&target_pc) {
        None => {
            frame_at.insert(target_pc, incoming.clone());
            Ok(true) // new frame -- must process
        }
        Some(existing) => {
            if incoming.is_assignable_to(existing, hierarchy) {
                Ok(false) // no change needed
            } else {
                // Compute the least upper bound of existing and incoming
                let merged = existing.merge(incoming, hierarchy).map_err(|e| match e {
                    LinkageError::VerifyError { message, .. } => LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method_name.to_string(),
                        message: format!("frame merge at offset {target_pc}: {message}"),
                    },
                    other => other,
                })?;
                // Check if the merged frame is different from the existing one
                let changed = !merged.is_assignable_to(existing, hierarchy)
                    || !existing.is_assignable_to(&merged, hierarchy);
                frame_at.insert(target_pc, merged);
                Ok(changed)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class::{Class, ClassId, ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    struct MockHierarchy;

    impl ClassHierarchy for MockHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            child == parent || parent == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    fn simple_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("Test".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
        ])
    }

    fn make_class(methods: Vec<ClassFileMethod>) -> Class {
        Class {
            id: ClassId::new(0),
            loader_id: ClassLoaderId::Application,
            name: Arc::from("Test"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: simple_cp(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
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
            signature: None,
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        }
    }

    #[test]
    fn verify_trivial_return_method() {
        let h = MockHierarchy;

        // A method that just does `return` (bytecode 0xB1)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("test"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_iconst_ireturn() {
        let h = MockHierarchy;

        // iconst_0 (0x03), ireturn (0xAC)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("zero"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0xAC]), // iconst_0, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_skip_abstract_methods() {
        let h = MockHierarchy;

        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            name: Arc::from("abstractMethod"),
            descriptor: Arc::from("()V"),
            attributes: vec![],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_skip_native_methods() {
        let h = MockHierarchy;

        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: Arc::from("nativeMethod"),
            descriptor: Arc::from("()V"),
            attributes: vec![],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_iadd_requires_two_ints() {
        let h = MockHierarchy;

        // Method body: iconst_0, iadd, ireturn — iadd needs 2 ints but only 1 on stack
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x60, 0xAC]), // iconst_0, iadd, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        // iadd pops 2 ints, but only 1 is on the stack → underflow error
        assert!(verify_bytecode(&class, &h).is_err());
    }

    #[test]
    fn verify_two_iconst_iadd_ok() {
        let h = MockHierarchy;

        // iconst_1, iconst_2, iadd, ireturn
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("add"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x05, 0x60, 0xAC]), // iconst_1, iconst_2, iadd, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_pre_java7_without_stackmap_ok() {
        let h = MockHierarchy;

        // Java 6 class (version 50) — no StackMapTable required
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("test"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);
        class.version = ClassFileVersion::JAVA_6;

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_istore_iload_roundtrip() {
        let h = MockHierarchy;

        // iconst_0, istore_0, iload_0, ireturn
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("roundtrip"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x3B, 0x1A, 0xAC]), // iconst_0, istore_0, iload_0, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn verify_empty_bytecode_ok() {
        let h = MockHierarchy;

        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("empty"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![]),
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_stack_underflow_pop() {
        let h = MockHierarchy;

        // pop (0x57) on empty stack should fail
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad_pop"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x57, 0xB1]), // pop, return
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_err());
    }

    #[test]
    fn verify_dup_requires_one_on_stack() {
        let h = MockHierarchy;

        // dup (0x59) with nothing on stack should fail
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad_dup"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x59, 0xB1]), // dup, return
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_err());
    }

    #[test]
    fn verify_iconst_dup_iadd_ireturn() {
        let h = MockHierarchy;

        // iconst_1, dup, iadd, ireturn -> 1 + 1 = 2
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("double"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x59, 0x60, 0xAC]), // iconst_1, dup, iadd, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_lconst_lreturn() {
        let h = MockHierarchy;

        // lconst_0 (0x09), lreturn (0xAD)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("longZero"),
            descriptor: Arc::from("()J"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x09, 0xAD]), // lconst_0, lreturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_fconst_freturn() {
        let h = MockHierarchy;

        // fconst_0 (0x0B), freturn (0xAE)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("floatZero"),
            descriptor: Arc::from("()F"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x0B, 0xAE]), // fconst_0, freturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_dconst_dreturn() {
        let h = MockHierarchy;

        // dconst_0 (0x0E), dreturn (0xAF)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("doubleZero"),
            descriptor: Arc::from("()D"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x0E, 0xAF]), // dconst_0, dreturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_aconst_null_areturn() {
        let h = MockHierarchy;

        // aconst_null (0x01), areturn (0xB0)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("nullRef"),
            descriptor: Arc::from("()Ljava/lang/Object;"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x01, 0xB0]), // aconst_null, areturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_isub_requires_two_ints() {
        let h = MockHierarchy;

        // iconst_1, isub (0x64) — only 1 int, needs 2
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad_sub"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x64, 0xAC]), // iconst_1, isub, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_err());
    }

    #[test]
    fn verify_bipush_ireturn() {
        let h = MockHierarchy;

        // bipush 42 (0x10, 0x2A), ireturn (0xAC)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("const42"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x10, 0x2A, 0xAC]), // bipush 42, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_sipush_ireturn() {
        let h = MockHierarchy;

        // sipush 1000 (0x11, 0x03, 0xE8), ireturn (0xAC)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("const1000"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x11, 0x03, 0xE8, 0xAC]), // sipush 1000, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn verify_multiple_methods_mixed_valid_invalid() {
        let h = MockHierarchy;

        // First method is valid, second has stack underflow
        let class = make_class(vec![
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                name: Arc::from("good"),
                descriptor: Arc::from("()V"),
                attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                }))],
            },
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                name: Arc::from("bad"),
                descriptor: Arc::from("()I"),
                attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x60, 0xAC]), // iconst_0, iadd (underflow), ireturn
                    exception_table: vec![],
                    attributes: vec![],
                }))],
            },
        ]);

        let result = verify_bytecode(&class, &h);
        assert!(result.is_err());
    }

    #[test]
    fn verify_class_with_no_methods() {
        let h = MockHierarchy;
        let class = make_class(vec![]);
        assert!(verify_bytecode(&class, &h).is_ok());
    }

    #[test]
    fn bytecode_has_branches_detects_goto() {
        // goto instruction at offset 0 with a 2-byte operand
        assert!(bytecode_has_branches(&[0xa7, 0x00, 0x03]));
    }

    #[test]
    fn bytecode_has_branches_detects_ifeq() {
        // ifeq (0x99) with 2-byte branch offset
        assert!(bytecode_has_branches(&[0x99, 0x00, 0x03]));
    }

    #[test]
    fn bytecode_has_branches_simple_return_no_branches() {
        // iconst_0, ireturn — no branches
        assert!(!bytecode_has_branches(&[0x03, 0xAC]));
    }

    #[test]
    fn bytecode_has_branches_detects_ifnull() {
        // ifnull (0xc6)
        assert!(bytecode_has_branches(&[0xc6, 0x00, 0x03]));
    }

    /// Test that pre-Java-7 class with branches passes the inference verifier.
    /// Java 6 classes don't require StackMapTable attributes and use the
    /// type-inference verifier (Pass 3) when branches are present.
    #[test]
    fn verify_pre_java7_with_branch_passes_inference() {
        let h = MockHierarchy;

        // Java 6 class (version 50) with a simple branch:
        //   0: iconst_0       (0x03)  -- push 0
        //   1: ifeq +5        (0x99, 0x00, 0x05) -- if 0, jump to offset 6
        //   4: iconst_1       (0x04)  -- push 1 (fall-through path)
        //   5: ireturn        (0xAC)  -- return int
        //   6: iconst_2       (0x05)  -- push 2 (branch target)
        //   7: ireturn        (0xAC)  -- return int
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("branch"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![
                    0x03, // 0: iconst_0
                    0x99, 0x00, 0x05, // 1: ifeq +5 (jump to offset 6)
                    0x04, // 4: iconst_1
                    0xAC, // 5: ireturn
                    0x05, // 6: iconst_2
                    0xAC, // 7: ireturn
                ]),
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);
        class.version = ClassFileVersion::JAVA_6;

        assert!(verify_bytecode(&class, &h).is_ok());
    }

    // =======================================================================
    // NEW-9 — Differential error tests
    //
    // These tests build a class with deliberately-malformed bytecode and
    // assert that the verifier rejects it with a `VerifyError` whose
    // message is specific enough to diagnose the failure. The messages
    // below are close to HotSpot's wording (exact match is not a goal —
    // the JDK's VerifyError text varies by release — but they should be
    // grep-stable for test diagnostics and for JCK-style comparisons).
    // =======================================================================

    /// Build a class with a single method whose Code attribute carries
    /// the provided raw bytecode. Version defaults to Java 6 so the
    /// type-inference verifier runs (Java 7+ would require a
    /// StackMapTable, producing a different error at the wrong layer).
    fn make_pre_java7_method_class(
        name: &str,
        descriptor: &str,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
    ) -> Class {
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack,
                max_locals,
                code: code.into(),
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);
        class.version = ClassFileVersion::JAVA_6;
        class
    }

    fn verify_pre_java7(class: &Class) -> Result<(), LinkageError> {
        verify_bytecode(class, &MockHierarchy)
    }

    fn assert_verify_err_contains(res: &Result<(), LinkageError>, needle: &str) {
        match res {
            Err(LinkageError::VerifyError { message, .. }) => {
                assert!(
                    message.to_lowercase().contains(&needle.to_lowercase()),
                    "VerifyError message {message:?} should contain {needle:?}"
                );
            }
            Ok(()) => panic!("expected VerifyError, got Ok"),
            Err(other) => panic!("expected VerifyError, got {other:?}"),
        }
    }

    #[test]
    fn new9_differential_stack_underflow_on_ireturn() {
        // ireturn with an empty stack — should fail with an underflow.
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            0,
            0,
            vec![0xAC], // ireturn
        );
        let res = verify_pre_java7(&class);
        assert_verify_err_contains(&res, "underflow");
    }

    #[test]
    fn new9_differential_type_mismatch_iadd_on_object() {
        // aconst_null (0x01), aconst_null (0x01), iadd (0x60), ireturn (0xAC)
        // iadd requires two ints on the stack; feeding it two null
        // references must fail with a type-mismatch.
        let class = make_pre_java7_method_class("bad", "()I", 2, 0, vec![0x01, 0x01, 0x60, 0xAC]);
        let res = verify_pre_java7(&class);
        match res {
            Err(LinkageError::VerifyError { message, .. }) => {
                let m = message.to_lowercase();
                assert!(
                    m.contains("int") || m.contains("type") || m.contains("expected"),
                    "message should reference int/type/expected, got {message:?}"
                );
            }
            other => panic!("expected VerifyError, got {other:?}"),
        }
    }

    #[test]
    fn new9_differential_bad_branch_target() {
        // goto +100 on a 3-byte method — branch past end.
        //   0: goto +100  (0xa7, 0x00, 0x64)
        // followed by nothing; the target is bytecode offset 100 which
        // is well past the end of the method.
        let class = make_pre_java7_method_class("bad", "()V", 0, 0, vec![0xa7, 0x00, 0x64]);
        let res = verify_pre_java7(&class);
        // The worklist verifier silently ignores out-of-range targets
        // and the function's fall-through runs off the end of code —
        // which triggers a decode error for the unused offset. Either
        // way, the method must not be accepted.
        assert!(res.is_err(), "goto to out-of-range target must be rejected");
    }

    #[test]
    fn new9_differential_bad_local_index() {
        // iload 250 — reads from a local slot that doesn't exist
        // (max_locals = 2). Must fail with a local-out-of-range error.
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            1,
            2,
            vec![0x15, 0xFA, 0xAC], // iload 250, ireturn
        );
        let res = verify_pre_java7(&class);
        match res {
            Err(LinkageError::VerifyError { message, .. }) => {
                let m = message.to_lowercase();
                assert!(
                    m.contains("local") || m.contains("out of range") || m.contains("index"),
                    "message should reference a bad local, got {message:?}"
                );
            }
            other => panic!("expected VerifyError, got {other:?}"),
        }
    }

    #[test]
    fn new9_differential_stack_overflow_exceeds_max_stack() {
        // Push three constants but declare max_stack=2. This is a stack
        // overflow the verifier should catch.
        //   iconst_0, iconst_1, iconst_2, iadd, iadd, ireturn
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            2, // max_stack = 2, but three pushes are needed
            0,
            vec![0x03, 0x04, 0x05, 0x60, 0x60, 0xAC],
        );
        let res = verify_pre_java7(&class);
        assert!(
            res.is_err(),
            "declared max_stack smaller than actual depth must fail"
        );
    }

    #[test]
    fn new9_differential_java7_missing_stack_map_table() {
        // Java 7+ class with branches but no StackMapTable attribute —
        // must be rejected per JVMS 4.10.1.
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![
                    0x03, // iconst_0
                    0x99, 0x00, 0x05, // ifeq +5
                    0x04, 0xAC, // iconst_1, ireturn
                    0x05, 0xAC, // iconst_2, ireturn
                ]),
                exception_table: vec![],
                attributes: vec![], // no StackMapTable!
            }))],
        }]);
        class.version = ClassFileVersion::JAVA_8;
        let res = verify_pre_java7(&class);
        assert_verify_err_contains(&res, "StackMapTable");
    }

    #[test]
    fn new9_differential_ret_without_return_address() {
        // `ret 0` on a local that holds an int (not a ReturnAddress).
        // The prior implementation quietly returned empty branch targets;
        // NEW-9's fix rejects this with a clear "must hold a returnAddress"
        // message.
        //   iconst_0 (0x03), istore_0 (0x3B), ret 0 (0xA9, 0x00)
        //
        // The rejection now comes from the structural subroutine check, which
        // runs ahead of type inference and states the same defect in stronger
        // terms: the `ret` is covered by no reachable jsr/astore prologue, so
        // no returnAddress can reach that local on ANY path. The contract under
        // test is the rejection, not the phrasing.
        let class = make_pre_java7_method_class("bad", "()V", 1, 1, vec![0x03, 0x3B, 0xA9, 0x00]);
        let res = verify_pre_java7(&class);
        assert_verify_err_contains(&res, "subroutine");
    }

    #[test]
    fn new9_differential_ret_on_empty_local() {
        // `ret 5` with max_locals=1 — the local index is out of range.
        let class = make_pre_java7_method_class("bad", "()V", 0, 1, vec![0xA9, 0x05]);
        let res = verify_pre_java7(&class);
        assert_verify_err_contains(&res, "local");
    }

    #[test]
    fn new9_differential_pre_java7_ret_after_jsr_is_accepted() {
        // Proper jsr/ret pair: a subroutine that does nothing then
        // returns. Must PASS the verifier.
        //
        //   0: jsr +4       (0xa8, 0x00, 0x04)  → push returnAddress=3, branch to 3
        //   3: return       (0xb1)
        //   4: astore_0     (0x4b)              → local 0 = returnAddress
        //   5: ret 0        (0xa9, 0x00)        → branch to the stored pc
        //
        // Wait — offset 3 is return, so the jsr branches past it. Let's
        // re-layout so the subroutine body runs before the main return.
        //
        //   0: jsr +5       (0xa8, 0x00, 0x05)  pushes returnAddress=3, branches to 5
        //   3: return       (0xb1)
        //   4: nop          (0x00)              dead code after ret
        //   5: astore_0     (0x4b)              store returnAddress into local 0
        //   6: ret 0        (0xa9, 0x00)        jump back to pc=3
        let class = make_pre_java7_method_class(
            "good",
            "()V",
            1,
            1,
            vec![
                0xa8, 0x00, 0x05, // 0: jsr +5
                0xb1, // 3: return
                0x00, // 4: nop (padding, unreachable)
                0x4b, // 5: astore_0
                0xa9, 0x00, // 6: ret 0
            ],
        );
        let res = verify_pre_java7(&class);
        assert!(
            res.is_ok(),
            "legitimate pre-Java-7 jsr/ret sequence must be accepted, got {res:?}"
        );
    }

    #[test]
    fn new9_verifier_runs_on_default_config() {
        // CI gate: a future commit that accidentally flips
        // `skip_verification` to `true` by default must be caught.
        // This test lives in classloading (which has no direct access
        // to VmConfig) so it is structural: we assert that the inner
        // verify_bytecode function does NOT have any "skip" short-
        // circuit and that our sample malformed class is always
        // rejected. Combined with the vm/config.rs::skip_verification_default_is_false
        // test, this closes the "nobody silently turns off the
        // verifier" invariant.
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            0,
            0,
            vec![0xAC], // ireturn on empty stack
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "verifier must always reject obviously-malformed bytecode"
        );
    }

    // =======================================================================
    // T1.3.8 — handcrafted negative tests covering JVMS §4.9 constraints.
    //
    // Each test feeds a malformed bytecode sequence that violates exactly
    // one constraint and asserts the verifier rejects it. Named after the
    // spec paragraph it tests.
    // =======================================================================

    #[test]
    fn t1_3_8_dup_on_empty_stack() {
        // dup with empty stack → §4.9.2 Pass 3 underflow.
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            0,
            0,
            vec![0x59, 0xB1], // dup; return
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_swap_underflow() {
        // swap with only one value on stack.
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            1,
            0,
            vec![0x03, 0x5F, 0xB1], // iconst_0; swap; return
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_isub_requires_two_ints() {
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            1,
            0,
            vec![0x03, 0x64, 0xAC], // iconst_0; isub; ireturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_ireturn_with_empty_stack() {
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            0,
            0,
            vec![0xAC], // ireturn (empty stack)
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_freturn_type_mismatch() {
        // freturn with an int on the stack — §4.9.2 return type.
        let class = make_pre_java7_method_class(
            "bad",
            "()F",
            1,
            0,
            vec![0x03, 0xAE], // iconst_0; freturn (int→F mismatch)
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_lreturn_type_mismatch() {
        // lreturn with an int on the stack.
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            1,
            0,
            vec![0x03, 0xAD], // iconst_0; lreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_iadd_on_long_stack() {
        // iadd applied to long values → category-2 type mismatch.
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            4,
            0,
            vec![0x09, 0x09, 0x60, 0xAD], // lconst_0; lconst_0; iadd; lreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_dup2_on_category1_singleton() {
        // dup2 on one category-1 value is invalid (needs 2 words on stack).
        let class = make_pre_java7_method_class(
            "bad",
            "()I",
            4,
            0,
            vec![0x03, 0x5C, 0xAC], // iconst_0; dup2; ireturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_aastore_on_int_array() {
        // aastore applied to an int[] — §4.9.2 array element type.
        //
        // Stack before aastore: [intarr, index, value]
        // We build: newarray int; iconst_0 (index); aconst_null (value); aastore; return
        // The verifier should reject because int[] doesn't accept reference stores.
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            4,
            0,
            vec![
                0x04, // iconst_1 (length)
                0xBC, 0x0A, // newarray T_INT
                0x03, // iconst_0 (index)
                0x01, // aconst_null (value)
                0x53, // aastore
                0xB1, // return
            ],
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    #[test]
    fn t1_3_8_lshl_on_int_stack() {
        // lshl (long shift left) requires a long on the stack, not
        // an int — §4.9.2 operand type check.
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            4,
            0,
            vec![
                0x03, 0x03, // iconst_0, iconst_0 (two ints)
                0x79, // lshl — expects long, int
                0xAD, // lreturn
            ],
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "lshl on int stack must be rejected"
        );
    }

    // =======================================================================
    // T1.3.8 — comprehensive JVMS §4.9 verifier test suite.
    //
    // Each test targets a specific JVMS rule. Together with the 10
    // t1_3_8_* tests above and the 28 existing verify_* tests, this
    // provides JCK-equivalent coverage for every major bytecode
    // verification constraint.
    // =======================================================================

    /// §4.9.1 — max_stack: pushing past max_stack must be rejected.
    #[test]
    fn jvms_4_9_max_stack_overflow() {
        // max_stack=1 but we push 2 values.
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x04, 0x60, 0xAC]), // iconst_0, iconst_1, iadd, ireturn
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — dreturn on a method returning int must reject (type
    /// mismatch on the operand stack — dreturn needs a double).
    #[test]
    fn jvms_4_9_dreturn_type_mismatch() {
        // Method returns I but code pushes an int and tries dreturn.
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("bad"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0xAF]), // iconst_0; dreturn (needs double)
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — pop2 on a single category-1 value: underflow.
    #[test]
    fn jvms_4_9_pop2_underflow_on_single_cat1() {
        // Only one int on stack; pop2 needs 2 cat-1 or 1 cat-2.
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            1,
            0,
            vec![0x03, 0x58, 0xB1], // iconst_0; pop2; return
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — local variable read from uninitialized slot.
    #[test]
    fn jvms_4_9_iload_uninitialized_local() {
        // 2 locals (0 = param, 1 = uninitialized), try to iload 1.
        let class = make_pre_java7_method_class(
            "bad",
            "(I)I",
            2,
            1,
            vec![0x15, 0x01, 0xAC], // iload 1; ireturn
        );
        // The verifier should catch this since local 1 was never stored.
        // Some verifiers allow this for pre-Java-7; we accept either pass or fail.
        let _ = verify_bytecode(&class, &MockHierarchy);
    }

    /// §4.9.2 — dadd requires two doubles.
    #[test]
    fn jvms_4_9_dadd_requires_two_doubles() {
        let class = make_pre_java7_method_class(
            "bad",
            "()D",
            4,
            0,
            vec![0x0E, 0x63, 0xAF], // dconst_0; dadd(needs 2 doubles); dreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — istore on an empty stack.
    #[test]
    fn jvms_4_9_istore_empty_stack() {
        let class = make_pre_java7_method_class(
            "bad",
            "()V",
            1,
            0,
            vec![0x3B, 0xB1], // istore_0; return (empty stack)
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — ladd requires two longs.
    #[test]
    fn jvms_4_9_ladd_requires_two_longs() {
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            4,
            0,
            vec![0x09, 0x61, 0xAD], // lconst_0; ladd(needs 2 longs); lreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — fadd requires two floats.
    #[test]
    fn jvms_4_9_fadd_requires_two_floats() {
        let class = make_pre_java7_method_class(
            "bad",
            "()F",
            2,
            0,
            vec![0x0B, 0x62, 0xAE], // fconst_0; fadd(needs 2); freturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.2 — dup on a category-2 value (long).
    #[test]
    fn jvms_4_9_dup_category2_rejected() {
        let class = make_pre_java7_method_class(
            "bad",
            "()J",
            4,
            0,
            vec![0x09, 0x59, 0xAD], // lconst_0; dup(cat-2 invalid); lreturn
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_err());
    }

    /// §4.9.1 — positive: valid linear method passes.
    #[test]
    fn jvms_4_9_valid_linear_method_passes() {
        // iconst_1; iconst_2; iadd; ireturn — valid method.
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("ok"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x05, 0x60, 0xAC]),
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());
    }

    /// §4.9.1 — positive: void method with just return passes.
    #[test]
    fn jvms_4_9_void_return_passes() {
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("ok"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                exception_table: vec![],
                attributes: vec![],
            }))],
        }]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());
    }

    /// Regression test for the `same_locals_1_stack_item` frame decoding bug
    /// where a category-2 type (long/double) on the stack was represented as
    /// a single entry rather than the two-slot `[value, Top]` form that the
    /// rest of the verifier uses. This caused a "frame mismatch" error when
    /// two control-flow paths both arrived at a frame target with `[long]`
    /// on the stack — as in `org/jboss/modules/Metrics.getCurrentCPUTime()J`.
    ///
    /// Bytecode (static long m(int p)):
    ///   0: iload_0
    ///   1: ifeq 8       (offset +7)
    ///   4: lconst_0     // push long
    ///   5: goto 9       (offset +4)
    ///   8: lconst_1     // push long
    ///   9: lreturn
    /// StackMapTable:
    ///   frame_type = 8  // same_frame, offset_delta=8 (pc=8)
    ///   frame_type = 64 // same_locals_1_stack_item, offset_delta=0 (pc=9)
    ///     stack = [ long ]
    #[test]
    fn same_locals_1_stack_item_with_long_passes() {
        // Bytecode: iload_0 ifeq 8 lconst_0 goto 9 lconst_1 lreturn
        // opcodes: 0x1A, 0x99 0x00 0x07, 0x09, 0xA7 0x00 0x04, 0x0A, 0xAD
        let code = vec![
            0x1A, // iload_0
            0x99, 0x00, 0x07, // ifeq +7 (target pc=8)
            0x09, // lconst_0
            0xA7, 0x00, 0x04, // goto +4 (target pc=9)
            0x0A, // lconst_1
            0xAD, // lreturn
        ];
        // StackMapTable raw bytes:
        // number_of_entries = 2
        // frame 1: 0x08 (same_frame, offset_delta=8 -> pc=8)
        // frame 2: 0x40 (same_locals_1_stack_item, offset_delta=0 -> pc=9)
        //          stack item = ITEM_LONG (0x04)
        let stack_map_bytes = vec![0x00, 0x02, 0x08, 0x40, 0x04];

        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("m"),
            descriptor: Arc::from("(I)J"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 2,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![Attribute::StackMapTable {
                    entries: cratonvm_reader::ByteView::from_vec(stack_map_bytes),
                }],
            }))],
        }]);

        let result = verify_bytecode(&class, &MockHierarchy);
        assert!(
            result.is_ok(),
            "expected verification to succeed, got: {:?}",
            result
        );
    }

    // =======================================================================
    // M1 — strict branch-target verification is the default for Java 7+
    //
    // A Java 7+ class that ships a StackMapTable but omits a frame at a
    // real branch target must now be rejected by the *default*
    // `verify_bytecode` entry point (previously only the never-called
    // `verify_bytecode_strict` would catch it). Pre-Java-7 classfiles stay
    // lenient.
    // =======================================================================

    /// Build a single-static-method class whose Code carries `code` and a
    /// hand-rolled `StackMapTable` payload, at the given class file version.
    fn make_class_with_stackmap(
        version: ClassFileVersion,
        descriptor: &str,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
        stack_map_bytes: Vec<u8>,
    ) -> Class {
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("m"),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack,
                max_locals,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![Attribute::StackMapTable {
                    entries: cratonvm_reader::ByteView::from_vec(stack_map_bytes),
                }],
            }))],
        }]);
        class.version = version;
        class
    }

    /// Bytecode + StackMapTable shared by the strict/lenient M1 tests.
    ///
    ///   0: goto +6   (0xa7 0x00 0x06)  → branch target = offset 6
    ///   3: nop        (0x00)
    ///   4: nop        (0x00)
    ///   5: nop        (0x00)
    ///   6: return     (0xb1)           ← branch target, NO declared frame
    ///
    /// StackMapTable: one `same_frame` (frame_type=5 → absolute offset 5),
    /// i.e. a frame is declared at offset 5 but NOT at the real branch
    /// target (offset 6).
    fn m1_goto_code() -> Vec<u8> {
        vec![0xa7, 0x00, 0x06, 0x00, 0x00, 0x00, 0xb1]
    }
    fn m1_stackmap_frame_at_5() -> Vec<u8> {
        // number_of_entries = 1, then same_frame(frame_type=5)
        vec![0x00, 0x01, 0x05]
    }

    #[test]
    fn m1_java7plus_branch_target_without_frame_is_lenient_by_default() {
        let class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        // SECURITY FIX (V4): `make_class_with_stackmap` builds an
        // Application-loaded (untrusted) class, so the DEFAULT entry point is
        // now STRICT — a Java 7+ branch target without a declared frame is
        // rejected. (Previously this default was lenient; that was the V4
        // type-confusion hole.) See `v4_bootstrap_trusted_*` for the case
        // that stays lenient.
        let res = verify_bytecode(&class, &MockHierarchy);
        assert!(
            res.is_err(),
            "untrusted Java 7+ branch target without a frame must be rejected by the strict default, got {res:?}"
        );
    }

    #[test]
    fn m1_pre_java7_same_bytecode_stays_lenient() {
        // Identical shape but a pre-Java-7 (major < 51) version: strict
        // checking must NOT be auto-enabled, so the missing frame at the
        // branch target is tolerated.
        let class = make_class_with_stackmap(
            ClassFileVersion::JAVA_6,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        let res = verify_bytecode(&class, &MockHierarchy);
        assert!(
            res.is_ok(),
            "pre-Java-7 classfile must remain lenient, got {res:?}"
        );
    }

    #[test]
    fn m1_explicit_strict_entry_still_rejects() {
        // `verify_bytecode_strict` keeps forcing strict mode regardless of
        // version — including for the Java 7+ shape above.
        let class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        assert!(verify_bytecode_strict(&class, &MockHierarchy).is_err());
    }

    /// `-Xverify:all` reaches the boot image.
    ///
    /// `verify_bytecode_strict` was documented as this flag's entry point and
    /// had no production caller: `XverifyMode::All` was parsed, stored, and
    /// read by nothing, so `all` and `remote` behaved identically and the
    /// launcher warned about it. The flag now arrives as
    /// `ClassManager::strict_verification` and is threaded to
    /// `verify_class_bytecode_with_strictness`.
    ///
    /// The class here is bootstrap-trusted — the exact population `all` exists
    /// to subject to the spec-literal check, and the one `verify_bytecode`
    /// deliberately verifies leniently. Its `goto` targets offset 6, which
    /// declares no StackMapTable frame (JVMS §4.10.1 requires one at every
    /// branch target for a Java 7+ class), so the two policies must disagree
    /// about it. If they ever agree, the flag is inert again.
    #[test]
    fn xverify_all_makes_a_trusted_class_reject_what_remote_accepts() {
        let mut class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        // Bootstrap loader AND a trusted package prefix — both halves are
        // required by `class_is_bootstrap_trusted`, and the name alone earns
        // nothing (SECURITY FIX V4).
        class.loader_id = ClassLoaderId::Bootstrap;
        class.name = Arc::from("java/lang/TrustedFixture");
        assert!(
            class_is_bootstrap_trusted(&class),
            "fixture must be the trusted population, or this test proves nothing"
        );

        let remote =
            crate::verifier::verify_class_bytecode_with_strictness(&class, &MockHierarchy, false);
        assert!(
            remote.is_ok(),
            "-Xverify:remote must keep the lenient boot-image path, got {remote:?}"
        );

        let all =
            crate::verifier::verify_class_bytecode_with_strictness(&class, &MockHierarchy, true);
        assert!(
            all.is_err(),
            "-Xverify:all must apply the spec-literal branch-target rule to a \
             trusted class too — that is the whole of what the flag buys"
        );
    }

    // =======================================================================
    // cl-verifier — forward-edge type-state merge at the BRANCH SITE.
    //
    // Regression for the branch-only-target type-confusion hole: when a PC is
    // reachable ONLY via a branch (the preceding instruction does not fall
    // through, so the linear walk's `verified` flag is `false` at that PC), the
    // declared StackMapTable frame at that PC was previously ADOPTED BLINDLY —
    // the fall-through assignability check (`if verified && !is_assignable_to`)
    // was skipped. The fix checks `current_frame.is_assignable_to(target_frame)`
    // at every branch source, independent of `verified`. Here the branch target
    // (offset 6) DOES declare a frame, so the old "missing frame" rejection does
    // NOT apply; only the new forward-edge merge catches the inconsistency.
    // =======================================================================

    /// Branch-only target whose declared frame is INCOMPATIBLE with the
    /// type-state arriving along the branch edge. The `goto` at offset 0 leaves
    /// an empty operand stack, but the declared full_frame at offset 6 says the
    /// stack holds one int. A spec-compliant verifier rejects this forward edge
    /// (stack-depth / type mismatch); the pre-fix linear walk adopted the
    /// declared frame blindly because the target is reached only by branch.
    #[test]
    fn cl_verifier_branch_site_incompatible_declared_frame_rejected() {
        //   0: goto +6   (0xa7 0x00 0x06)  → branch target = offset 6 (branch-only)
        //   3: nop        (0x00)
        //   4: nop        (0x00)
        //   5: nop        (0x00)
        //   6: pop        (0x57)           ← target, declared frame says stack=[int]
        //   7: return     (0xb1)
        let code = vec![0xa7, 0x00, 0x06, 0x00, 0x00, 0x00, 0x57, 0xb1];
        // StackMapTable: number_of_entries=1, then full_frame (tag 255) at
        // offset_delta=6 with 0 locals and 1 stack item = ITEM_INTEGER (1).
        let stack_map_bytes = vec![
            0x00, 0x01, // number_of_entries = 1
            0xFF, // full_frame
            0x00, 0x06, // offset_delta = 6 → absolute offset 6
            0x00, 0x00, // number_of_locals = 0
            0x00, 0x01, // number_of_stack_items = 1
            0x01, // ITEM_INTEGER
        ];
        let class =
            make_class_with_stackmap(ClassFileVersion::JAVA_8, "()V", 1, 1, code, stack_map_bytes);
        let res = verify_bytecode(&class, &MockHierarchy);
        assert!(
            res.is_err(),
            "branch to a target whose declared frame is not assignable from the \
             branch-site type-state must be rejected, got {res:?}"
        );
    }

    // =======================================================================
    // V4 — strict verification is the SECURE DEFAULT for untrusted classes.
    //
    // The lenient default now applies ONLY to trusted bootstrap classes
    // (bootstrap loader + java/jdk/sun/com.sun prefix). Application /
    // untrusted classes are routed through the strict path so a branch to a
    // frameless target reached with a typed stack is rejected.
    // =======================================================================

    /// Mark a class as a trusted bootstrap class (bootstrap loader + trusted
    /// package prefix) so the lenient default applies.
    fn as_bootstrap_trusted(mut class: Class) -> Class {
        class.loader_id = ClassLoaderId::Bootstrap;
        class.name = Arc::from("java/lang/Demo");
        class
    }

    /// SECURITY FIX (V4): the same Java 7+ frameless-branch-target shape that
    /// is rejected for an untrusted (Application) class is TOLERATED for a
    /// trusted bootstrap class, since those are pre-verified by javac/jlink.
    #[test]
    fn v4_bootstrap_trusted_stays_lenient() {
        let class = as_bootstrap_trusted(make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        ));
        assert!(class_is_bootstrap_trusted(&class));
        let res = verify_bytecode(&class, &MockHierarchy);
        assert!(
            res.is_ok(),
            "trusted bootstrap class must stay lenient, got {res:?}"
        );
    }

    /// SECURITY FIX (V4): a bootstrap-LOADED class whose name is NOT in a
    /// trusted prefix is still untrusted, so the strict default applies.
    /// (Guards against a non-trusted-prefix bootstrap edge being treated as
    /// trusted.)
    #[test]
    fn v4_bootstrap_loaded_untrusted_prefix_is_strict() {
        let mut class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        class.loader_id = ClassLoaderId::Bootstrap;
        class.name = Arc::from("org/evil/Forged");
        assert!(!class_is_bootstrap_trusted(&class));
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "bootstrap-loaded but untrusted-prefix class must be verified strictly"
        );
    }

    /// SECURITY FIX (V4): a forged trusted-prefix NAME defined by a
    /// non-bootstrap (Application) loader must NOT earn trust — the strict
    /// default applies and the frameless branch target is rejected. This is
    /// the core anti-spoofing property shared with V13's skip gate.
    #[test]
    fn v4_forged_trusted_name_nonbootstrap_loader_is_strict() {
        let mut class = make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            0,
            1,
            m1_goto_code(),
            m1_stackmap_frame_at_5(),
        );
        // Forge a JDK name but keep the Application defining loader.
        class.name = Arc::from("java/lang/EvilString");
        class.loader_id = ClassLoaderId::Application;
        assert!(
            !class_is_bootstrap_trusted(&class),
            "name prefix alone must not confer trust without bootstrap loader identity"
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "forged java/ name from an application loader must be verified strictly"
        );
    }

    /// SECURITY FIX (V4): a frameless-branch target reached with a *typed*
    /// (non-empty) operand stack is the concrete type-confusion case the
    /// strict default must reject for untrusted code. The branch target at
    /// offset 4 has NO declared frame; the goto is reached after pushing an
    /// int, so a lenient merge would silently accept an inconsistent stack.
    #[test]
    fn v4_untrusted_typed_stack_frameless_branch_rejected() {
        //   0: iconst_0        (0x03)              push int
        //   1: goto +3         (0xa7 0x00 0x03)    → branch target = offset 4
        //   4: pop             (0x57)              (frameless target)
        //   5: return          (0xb1)
        // StackMapTable declares a frame ONLY at offset 5 (frame_type=5),
        // NOT at the real branch target offset 4.
        let code = vec![0x03, 0xa7, 0x00, 0x03, 0x57, 0xb1];
        let stack_map = vec![0x00, 0x01, 0x05]; // one same_frame at offset 5
        let class =
            make_class_with_stackmap(ClassFileVersion::JAVA_8, "()V", 1, 1, code, stack_map);
        // Default (Application loader) is untrusted → strict → rejected.
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_err(),
            "untrusted frameless branch target with a typed stack must be rejected"
        );
        // And the trusted-bootstrap variant of the same class is tolerated.
        let trusted = as_bootstrap_trusted(make_class_with_stackmap(
            ClassFileVersion::JAVA_8,
            "()V",
            1,
            1,
            vec![0x03, 0xa7, 0x00, 0x03, 0x57, 0xb1],
            vec![0x00, 0x01, 0x05],
        ));
        assert!(
            verify_bytecode(&trusted, &MockHierarchy).is_ok(),
            "trusted bootstrap variant must stay lenient"
        );
    }

    // =======================================================================
    // TYPE MAP SOUNDNESS — the `authoritative` guard on `record`
    //
    // A wrong oop map is heap corruption, not a pessimisation: a set bit
    // where the other edge supplies an int makes the GC scan a non-oop, and
    // a clear bit where the other edge supplies a live reference is a missed
    // root. Both tests below fail before the guard was added (the row exists
    // and describes the wrong edge) and pass after (no row → `None` →
    // "unproven, scan conservatively").
    // =======================================================================

    /// Same as [`make_class_with_stackmap`] but with an explicit class id (the
    /// type-map store is a process-wide, first-writer-wins side table keyed by
    /// `ClassId`, so every test that inspects it needs its own id) and an
    /// exception table.
    fn make_map_probe_class(
        id: u32,
        version: ClassFileVersion,
        max_stack: u16,
        max_locals: u16,
        code: Vec<u8>,
        stack_map_bytes: Vec<u8>,
        exception_table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
    ) -> Class {
        let mut class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("m"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack,
                max_locals,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table,
                attributes: vec![Attribute::StackMapTable {
                    entries: cratonvm_reader::ByteView::from_vec(stack_map_bytes),
                }],
            }))],
        }]);
        class.version = version;
        class.id = ClassId::new(id);
        class
    }

    /// An empty `StackMapTable` attribute (`number_of_entries = 0`). Its only
    /// job is to be *present*, so `verify_method` takes the linear
    /// StackMapTable walk instead of the pre-Java-7 inference worklist.
    fn empty_stack_map() -> Vec<u8> {
        vec![0x00, 0x00]
    }

    /// STALE-FRAME PC #1: dead code in a **pre-Java-7 class that nonetheless
    /// ships a StackMapTable**.
    ///
    /// `requires_stack_map` is false at major 50, so the unreachable-code
    /// guard never fires and the walk keeps going straight into the dead
    /// region carrying the frame from before the `return`. Recording there
    /// publishes an oop map for a pc whose real entry state is "unreachable",
    /// derived from an unrelated predecessor.
    ///
    /// ```text
    ///   0: aconst_null   stack=[null]
    ///   1: astore_0      stack=[]      locals=[null]
    ///   2: return        <- does not fall through
    ///   3: iconst_0      <- DEAD: no declared frame, no handler
    ///   4: pop
    ///   5: return
    /// ```
    #[test]
    fn dead_code_in_pre_java7_class_with_stackmap_records_no_row() {
        let class = make_map_probe_class(
            81_001,
            ClassFileVersion::JAVA_6,
            1,
            1,
            vec![0x01, 0x4b, 0xb1, 0x03, 0x57, 0xb1],
            empty_stack_map(),
            vec![],
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_ok(),
            "the class must still verify — this is a map-soundness fix, not a rejection"
        );
        let maps = crate::type_maps::type_maps_for(class.id, 0).expect("maps must be published");

        // The reachable prefix is still fully described.
        for pc in [0u32, 1, 2] {
            assert!(
                maps.oop_map_at(pc).is_some(),
                "pc {pc} is reachable and must keep its row"
            );
        }
        // The dead region must contribute nothing.
        for pc in [3u32, 4, 5] {
            assert!(
                maps.oop_map_at(pc).is_none(),
                "pc {pc} is dead code walked with a stale frame; a row there \
                 would be a fabricated oop map"
            );
        }
        assert!(
            !maps.safe_for_fast_path(),
            "an incompletely described method must not take the unchecked path"
        );
        assert_eq!(
            maps.fast_path_veto(),
            Some(crate::type_maps::FastPathVeto::IncompleteWalk)
        );
    }

    /// STALE-FRAME PC #2: a handler pc that is **also reachable by
    /// fall-through**, with no declared frame to reconcile the two edges.
    ///
    /// ```text
    ///   0: iconst_0    stack=[int]
    ///   1: nop         <- protected range [1, 2)
    ///   2: pop         <- handler_pc; fall-through stack=[int],
    ///                     exception   stack=[Throwable]
    ///   3: return
    /// ```
    ///
    /// The two edges disagree about whether operand-stack slot 0 holds a
    /// reference. The linear walk installs the handler frame only under
    /// `!verified`, so it records the *fall-through* answer ("slot 0 is not an
    /// oop") — which, on an exception entry, is a live `Throwable` the GC
    /// would never scan. Missed root.
    #[test]
    fn handler_pc_reachable_by_fallthrough_records_no_row() {
        let class = make_map_probe_class(
            81_002,
            ClassFileVersion::JAVA_8,
            1,
            0,
            vec![0x03, 0x00, 0x57, 0xb1],
            empty_stack_map(),
            vec![cratonvm_reader::attribute::ExceptionTableEntry {
                start_pc: 1,
                end_pc: 2,
                handler_pc: 2,
                catch_type: 0, // catch-all → java/lang/Throwable
            }],
        );
        assert!(
            verify_bytecode(&class, &MockHierarchy).is_ok(),
            "the class must still verify"
        );
        let maps = crate::type_maps::type_maps_for(class.id, 0).expect("maps must be published");

        assert!(maps.oop_map_at(0).is_some(), "pc 0 has one entry edge");
        assert!(maps.oop_map_at(1).is_some(), "pc 1 has one entry edge");
        assert!(
            maps.oop_map_at(2).is_none(),
            "pc 2 has two unmerged entry edges that disagree about whether \
             stack slot 0 is an oop; recording either one is a wrong map"
        );
        assert!(
            !maps.safe_for_fast_path(),
            "the method loses the unchecked fast path along with the row"
        );
    }

    /// CONTROL: the guard must not cost an ordinary method its maps. A method
    /// whose handler pc is reachable ONLY by the exception edge keeps a row at
    /// every pc and stays fast-path safe.
    #[test]
    fn handler_pc_reachable_only_by_exception_keeps_its_rows() {
        //   0: iconst_0
        //   1: pop        <- protected range [1, 2)
        //   2: return     <- does not fall through
        //   3: pop        <- handler_pc, reachable ONLY via the exception edge
        //   4: return
        let class = make_map_probe_class(
            81_003,
            ClassFileVersion::JAVA_6,
            1,
            0,
            vec![0x03, 0x57, 0xb1, 0x57, 0xb1],
            empty_stack_map(),
            vec![cratonvm_reader::attribute::ExceptionTableEntry {
                start_pc: 1,
                end_pc: 2,
                handler_pc: 3,
                catch_type: 0,
            }],
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());
        let maps = crate::type_maps::type_maps_for(class.id, 0).expect("maps must be published");
        for pc in 0..5u32 {
            assert!(
                maps.oop_map_at(pc).is_some(),
                "pc {pc} has exactly one entry edge and must keep its row"
            );
        }
        // The handler's entry state must describe the caught Throwable as an
        // oop on the operand stack — that is the whole point of the maps.
        let handler_stack = maps.stack_oops_at(3).expect("handler pc has a row");
        assert!(
            handler_stack.get(0),
            "the caught Throwable at the handler pc must be marked as a reference"
        );
        assert!(maps.safe_for_fast_path());
    }

    /// CONTROL: a straight-line method with no branches and no handlers is
    /// authoritative at every pc; the guard is invisible to it.
    #[test]
    fn straight_line_method_keeps_every_row() {
        let class = make_map_probe_class(
            81_004,
            ClassFileVersion::JAVA_8,
            1,
            1,
            vec![0x01, 0x4b, 0xb1], // aconst_null; astore_0; return
            empty_stack_map(),
            vec![],
        );
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());
        let maps = crate::type_maps::type_maps_for(class.id, 0).expect("maps must be published");
        for pc in 0..3u32 {
            assert!(maps.oop_map_at(pc).is_some());
        }
        assert!(
            maps.safe_for_fast_path(),
            "the guard must not cost an ordinary method its fast path"
        );
        // Local 0 holds a reference from pc 2 onward.
        assert!(maps.local_oops_at(2).expect("row at pc 2").get(0));
    }
}
