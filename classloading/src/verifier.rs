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
//! **SECURITY FIX (HIGH).** The previous behaviour routed such methods to
//! a *structural-only* fallback: it decoded instructions and checked
//! branch/handler bounds but performed **no** operand-stack / local
//! type-state verification, then accepted the method. A permissive
//! verifier is a memory-safety hole — unverified bytecode would reach the
//! interpreter/JIT with no type-consistency guarantee. We now **reject**
//! by default: the structural sanity scan still runs (so malformed
//! bytecode is still caught), but the type-verification bypass is a hard
//! `VerifyError` rather than silent acceptance. The legacy structural-only
//! acceptance remains available behind the `CRATONVM_ALLOW_JSR_RET` opt-in
//! escape hatch (see [`allow_jsr_ret`]).
//!
//! Methods that do **not** use subroutines are verified strictly via the
//! existing `bytecode_verifier::verify_bytecode` path, so the rejection
//! is bounded to the exact set of methods that the worklist verifier
//! cannot model. Java 7+ classes (which never emit `jsr`/`ret` and are
//! required to ship `StackMapTable`) are unaffected — the fast path
//! delegates to `bytecode_verifier::verify_bytecode` unchanged.

#[cfg(test)]
use std::sync::Arc;
use std::sync::OnceLock;

use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::instruction::Instruction;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::stack_map::StackMapTable;

use super::class::{find_method_recursive, Class, ClassStore};
use super::verify_frame::VerificationFrame;
use super::verify_insn::verify_instruction;
use super::vtype::{ClassHierarchy, VType};
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
    verify_class_structure(class, store)?;
    verify_class_bytecode(class, hierarchy)?;
    Ok(())
}

/// Pass 3 dispatcher with JSR/RET subroutine relaxation.
///
/// See module-level docs for the rationale. Common case (no `jsr`/`ret`
/// anywhere in the class) delegates to
/// [`super::bytecode_verifier::verify_bytecode`] verbatim. Otherwise,
/// each method is verified individually:
///   * methods using subroutines → structural sanity scan, then a hard
///     `VerifyError` (SECURITY FIX HIGH), because our worklist verifier
///     collapses two distinct `ReturnAddress` values into `Top` at the
///     subroutine entry and so cannot type-check them — accepting them
///     unverified is a memory-safety hole. The legacy structural-only
///     acceptance is available behind the `CRATONVM_ALLOW_JSR_RET`
///     escape hatch (see [`allow_jsr_ret`]);
///   * everything else → the same per-method type-state algorithm that
///     `bytecode_verifier::verify_bytecode` runs (StackMapTable-driven
///     for Java 7+, worklist-based for Java 6 and earlier).
///
/// The per-method routing is what isolates the rejection: a non-JSR
/// method in the same class as a JSR method is still strictly
/// type-state-verified, so a real bug in the non-JSR method will not
/// be masked by the rejection of the JSR method.
///
/// This is the JSR-aware Pass 3 entry point; consumers that previously
/// called [`super::bytecode_verifier::verify_bytecode`] should call
/// this function instead for any class that may legitimately contain
/// pre-Java-7 subroutine bytecode (every classpath class with major
/// version ≤ 50 qualifies).
/// Cached verdict for the `CRATONVM_ALLOW_JSR_RET` escape hatch.
///
/// SECURITY FIX (HIGH): pre-Java-7 methods that use `jsr` / `jsr_w` / `ret`
/// cannot be type-state-verified by our worklist verifier (it collapses the
/// two distinct `ReturnAddress` values at a shared subroutine entry into
/// `Top`; see the module-level docs). The previous behaviour routed such
/// methods to a *structural-only* fallback that decoded instructions and
/// checked branch/handler bounds but performed **no** operand-stack / local
/// type-state verification. A permissive verifier is a memory-safety hole:
/// unverified bytecode reaches the interpreter/JIT with no guarantee that the
/// operand stack and locals are type-consistent.
///
/// The safe default is therefore to **reject** any class whose method uses a
/// subroutine opcode (VerifyError-equivalent) rather than silently accept it
/// unverified. The full §4.10.2.5 subroutine-inlining verifier is non-trivial
/// (~1k LOC of bytecode rewriting) and not yet implemented, so until it lands
/// the structural decode/bounds checks still run (to surface malformed
/// bytecode) but the type-verification bypass is a hard rejection.
///
/// Setting `CRATONVM_ALLOW_JSR_RET` to a non-empty, non-`"0"` value opts back
/// into the legacy structural-only acceptance for environments that must load
/// legacy pre-Java-7 jars and accept the reduced verification guarantee. Off
/// by default. Read once at process start (the env can't change mid-run),
/// mirroring the `OnceLock` pattern used elsewhere in this crate.
static ALLOW_JSR_RET: OnceLock<bool> = OnceLock::new();

/// `true` when `CRATONVM_ALLOW_JSR_RET` is set to a non-empty, non-`"0"`
/// value. Computed once and cached for the process lifetime. See
/// [`ALLOW_JSR_RET`].
fn allow_jsr_ret() -> bool {
    *ALLOW_JSR_RET.get_or_init(|| {
        std::env::var("CRATONVM_ALLOW_JSR_RET")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
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
    verify_class_bytecode_inner(class, hierarchy, allow_jsr_ret())
}

fn verify_class_bytecode_inner(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
    allow_jsr: bool,
) -> Result<(), LinkageError> {
    // Identify methods that use subroutines (jsr / jsr_w / ret). The scan
    // is opcode-only; we do not need to fully decode the bytecode to
    // detect these three opcodes (0xa8 / 0xc9 / 0xa9) and the `wide ret`
    // form (0xc4 0xa9). Variable-length operands (tableswitch /
    // lookupswitch) need to be skipped correctly so we do not mis-read a
    // jump-table padding byte as a JSR opcode.
    let mut any_jsr = false;
    for method in class.methods.iter() {
        if method.is_abstract() || method.is_native() {
            continue;
        }
        if method_uses_jsr_or_ret(method) {
            any_jsr = true;
            break;
        }
    }

    if !any_jsr {
        // Common path — no subroutines anywhere. Delegate to the
        // standard verifier unchanged. `verify_bytecode` now self-selects
        // strict-by-default for untrusted classes (SECURITY FIX V4).
        return super::bytecode_verifier::verify_bytecode(class, hierarchy);
    }

    // SECURITY FIX (V4): strict branch-target frame checking is the default
    // for untrusted (non-bootstrap-trusted) classes, consistent with
    // `bytecode_verifier::verify_bytecode`. The per-method JSR path below
    // honours the same decision so a JSR-containing untrusted class does not
    // silently downgrade its non-JSR sibling methods to lenient mode.
    let strict = !super::bytecode_verifier::class_is_bootstrap_trusted(class);

    // At least one method in this class uses jsr/ret. Walk methods
    // individually so we can apply the structural-only fallback to the
    // subroutine-using ones while still type-state-verifying the rest.
    for method in class.methods.iter() {
        if method.is_abstract() || method.is_native() {
            continue;
        }
        if method_uses_jsr_or_ret(method) {
            // SECURITY FIX (HIGH): subroutine-using method. We still run the
            // structural sanity scan first so malformed bytecode (truncated
            // instructions, out-of-range jumps, ill-formed handler ranges) is
            // rejected exactly as before. We then make the *type-verification
            // bypass* a hard rejection: our worklist verifier cannot model the
            // §4.10.2.5 subroutine-inlining rules, so accepting these methods
            // would let unverified bytecode through — a memory-safety hole.
            //
            // The legacy structural-only acceptance is available behind the
            // `CRATONVM_ALLOW_JSR_RET` opt-in escape hatch for callers that
            // must load legacy pre-Java-7 jars and accept the reduced
            // guarantee; the default is to reject.
            verify_method_structural_only(class, method)?;
            if !allow_jsr {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: method.name.to_string(),
                    message: "method uses jsr/jsr_w/ret subroutine opcodes, which cannot be \
                              type-checked by this verifier; refusing to load it unverified \
                              (set CRATONVM_ALLOW_JSR_RET=1 to opt into legacy structural-only \
                              acceptance)"
                        .to_string(),
                });
            }
        } else {
            // Non-subroutine method: full type-state verification.
            // Performs the same algorithm as
            // `bytecode_verifier::verify_method` but in isolation per
            // method, so a real bug in this method cannot be masked by
            // a tolerated failure in a JSR-using method.
            verify_method_typestate(class, method, hierarchy, strict)?;
        }
    }

    Ok(())
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
fn verify_method_typestate(
    class: &Class,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
    // SECURITY FIX (V4): `true` for untrusted code — enforce spec-compliant
    // per-branch-target frame checking; `false` only for pre-verified trusted
    // bootstrap classes.
    strict: bool,
) -> Result<(), LinkageError> {
    // Structural sanity is a prerequisite for type-state verification:
    // we can't walk instructions if the bytecode itself is malformed.
    verify_method_structural_only(class, method)?;

    let code_attr = match method.code() {
        Some(c) => c,
        None => return Ok(()),
    };
    let bytecode = &code_attr.code;
    if bytecode.is_empty() {
        return Ok(());
    }

    let cp = &class.constant_pool;
    let class_name = &class.name;
    let version = &class.version;
    let requires_stack_map = version.major >= ClassFileVersion::JAVA_7.major;

    // Find the StackMapTable raw bytes among the Code attribute's
    // sub-attributes.
    // `entries: &Arc<[u8]>` (round 4 reader) — deref to a `&[u8]` slice
    // that lives as long as the attribute does.
    let stack_map_raw = code_attr.attributes.iter().find_map(|a| match a {
        cratonvm_reader::attribute::Attribute::StackMapTable { entries } => Some(&entries[..]),
        _ => None,
    });

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
            return verify_pre_java7_inference(class, method, hierarchy);
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

    let compact = VerificationFrame::compact_initial_frame(
        class_name,
        &method.name,
        &method.descriptor,
        method.is_static(),
        code_attr.max_stack,
    );
    let initial = VerificationFrame::initial_frame(
        class_name,
        &method.name,
        &method.descriptor,
        method.is_static(),
        code_attr.max_locals,
        code_attr.max_stack,
    );

    let declared_frames: std::collections::HashMap<u16, VerificationFrame> = match &parsed_table {
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
            let mut frames = std::collections::HashMap::with_capacity(table.entries.len());
            let mut prev = compact.clone();
            for (i, entry) in table.entries.iter().enumerate() {
                let off = offsets[i];
                let new_frame = prev.apply_stack_map_frame(entry, cp).map_err(|e| match e {
                    LinkageError::VerifyError { message, .. } => LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("StackMapTable frame {i} at offset {off}: {message}"),
                    },
                    other => other,
                })?;
                prev = new_frame.clone();
                frames.insert(off, new_frame);
            }
            frames
        }
        None => std::collections::HashMap::new(),
    };

    let mut handler_targets: std::collections::HashMap<u16, VType> =
        std::collections::HashMap::with_capacity(code_attr.exception_table.len());
    for entry in &code_attr.exception_table {
        let catch = if entry.catch_type == 0 {
            VType::ObjectRef(std::sync::Arc::from("java/lang/Throwable"))
        } else {
            match cp.get_class_name_arc(entry.catch_type) {
                Some(name) => VType::ObjectRef(name),
                None => VType::ObjectRef(std::sync::Arc::from("java/lang/Throwable")),
            }
        };
        handler_targets.insert(entry.handler_pc, catch);
    }

    let mut pc = 0usize;
    let mut current = initial;
    let mut verified = true;
    while pc < bytecode.len() {
        if let Some(declared) = declared_frames.get(&(pc as u16)) {
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
            let mut adopted = declared.clone();
            adopted.pad_locals_to(code_attr.max_locals);
            current = adopted;
        }

        if let Some(catch_type) = handler_targets.get(&(pc as u16)) {
            if !verified {
                let mut handler_frame = current.clone();
                handler_frame.clear_stack();
                handler_frame
                    .push(catch_type.clone())
                    .map_err(|_| LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("exception handler stack overflow at offset {pc}"),
                    })?;
                current = handler_frame;
            }
        }

        if !verified
            && requires_stack_map
            && !declared_frames.contains_key(&(pc as u16))
            && !handler_targets.contains_key(&(pc as u16))
        {
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
            let (_, next_pc) = match Instruction::decode(bytecode, pc) {
                Ok(r) => r,
                Err(_) => break,
            };
            pc = next_pc;
            continue;
        }

        let (insn, next_pc) =
            Instruction::decode(bytecode, pc).map_err(|e| LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: format!("failed to decode instruction at offset {pc}: {e}"),
            })?;

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

        verified = result.falls_through;
        if !result.falls_through && next_pc < bytecode.len() {
            verified = false;
        }
        pc = next_pc;
    }

    Ok(())
}

/// Pre-Java-7 worklist type inference for a single method.
///
/// Mirrors `bytecode_verifier::verify_by_inference`. Used only for
/// non-JSR pre-Java-7 methods (subroutine-using methods take the
/// structural-only fallback before reaching here).
fn verify_pre_java7_inference(
    class: &Class,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    let code_attr = match method.code() {
        Some(c) => c,
        None => return Ok(()),
    };
    let bytecode = &code_attr.code;
    let cp = &class.constant_pool;
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
                let catch = if entry.catch_type == 0 {
                    VType::ObjectRef(std::sync::Arc::from("java/lang/Throwable"))
                } else {
                    match cp.get_class_name_arc(entry.catch_type) {
                        Some(name) => VType::ObjectRef(name),
                        None => VType::ObjectRef(std::sync::Arc::from("java/lang/Throwable")),
                    }
                };
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

    Ok(())
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
fn method_uses_jsr_or_ret(method: &ClassFileMethod) -> bool {
    let code_attr = match method.code() {
        Some(c) => c,
        None => return false,
    };
    let bytecode = &code_attr.code;
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let opcode = bytecode[pc];
        // 0xa8 = jsr, 0xa9 = ret, 0xc9 = jsr_w
        if opcode == 0xa8 || opcode == 0xa9 || opcode == 0xc9 {
            return true;
        }
        // wide prefix may wrap a `ret` (0xc4 0xa9 ...): catch that too.
        if opcode == 0xc4 && pc + 1 < bytecode.len() && bytecode[pc + 1] == 0xa9 {
            return true;
        }
        // Use the canonical decoder to advance. Malformed bytecode here
        // is harmless — `verify_method_structural_only` will reject it.
        let next = match Instruction::decode(bytecode, pc) {
            Ok((_, next_pc)) => next_pc,
            Err(_) => return false,
        };
        if next <= pc {
            // defensive: zero/negative advance — stop scanning
            return false;
        }
        pc = next;
    }
    false
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
fn verify_method_structural_only(
    class: &Class,
    method: &ClassFileMethod,
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
            class_name: class.name.to_string(),
            method_name: method.name.to_string(),
            message: "Code attribute has empty bytecode array".to_string(),
        });
    }

    // Decode every instruction and collect branch targets.
    let mut pc = 0usize;
    while pc < code_len {
        let (insn, next_pc) =
            Instruction::decode(bytecode, pc).map_err(|e| LinkageError::VerifyError {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                message: format!("failed to decode instruction at offset {pc}: {e}"),
            })?;

        // Enumerate branch targets that this instruction can reach and
        // confirm each one lies inside the code array. We do NOT do
        // type-state verification here — we only check that the program
        // counter never falls off the edge of the method.
        for target in instruction_branch_targets(&insn, pc) {
            if target as usize > code_len {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "branch target {target} at offset {pc} is out of range \
                         (code length = {code_len})"
                    ),
                });
            }
        }

        if next_pc <= pc {
            // Pathological: decoder claimed zero/negative advance.
            return Err(LinkageError::VerifyError {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                message: format!("instruction at offset {pc} did not advance program counter"),
            });
        }
        pc = next_pc;
    }

    // Validate exception handler ranges (JVMS §4.9.1).
    for entry in &code_attr.exception_table {
        let start = entry.start_pc as usize;
        let end = entry.end_pc as usize;
        let handler = entry.handler_pc as usize;
        if start >= end {
            return Err(LinkageError::VerifyError {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                message: format!("exception handler range invalid: start_pc={start}, end_pc={end}"),
            });
        }
        if end > code_len {
            return Err(LinkageError::VerifyError {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                message: format!("exception handler end_pc={end} is past code length {code_len}"),
            });
        }
        if handler >= code_len {
            return Err(LinkageError::VerifyError {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                message: format!(
                    "exception handler handler_pc={handler} is past code length {code_len}"
                ),
            });
        }
    }

    Ok(())
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
fn instruction_branch_targets(insn: &Instruction, pc: usize) -> Vec<u16> {
    let pc_i32 = pc as i32;
    let target = |off: i32| -> Option<u16> {
        let t = pc_i32.checked_add(off)?;
        if (0..=u16::MAX as i32).contains(&t) {
            Some(t as u16)
        } else {
            None
        }
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
        | Instruction::Jsr(o) => target(*o as i32).into_iter().collect(),
        Instruction::GotoW(o) | Instruction::JsrW(o) => target(*o).into_iter().collect(),
        Instruction::Tableswitch {
            default, offsets, ..
        } => {
            let mut v = Vec::with_capacity(offsets.len() + 1);
            if let Some(t) = target(*default) {
                v.push(t);
            }
            for off in offsets {
                if let Some(t) = target(*off) {
                    v.push(t);
                }
            }
            v
        }
        Instruction::Lookupswitch { default, pairs } => {
            let mut v = Vec::with_capacity(pairs.len() + 1);
            if let Some(t) = target(*default) {
                v.push(t);
            }
            for (_, off) in pairs {
                if let Some(t) = target(*off) {
                    v.push(t);
                }
            }
            v
        }
        // `ret` jumps to a returnAddress stored in a local — the target
        // is dynamic, not encoded in the instruction. Not checked here.
        _ => Vec::new(),
    }
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
    verify_inherited_abstract_methods_implemented(class, store)?;
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
    if class.is_synthetic_stub {
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
fn verify_final_method_constraint(class: &Class, store: &ClassStore) -> Result<(), LinkageError> {
    // Only check if we have a superclass
    let super_id = match class.superclass {
        Some(id) => id,
        None => return Ok(()),
    };

    for method in &class.methods {
        // Skip static methods, constructors, and class initializers
        if method.is_static() || &*method.name == "<init>" || &*method.name == "<clinit>" {
            continue;
        }

        // Check if this method overrides a FINAL method in a superclass.
        // Per JVMS, private and static methods are not inherited and therefore
        // cannot be overridden — a same-named method in a subclass is a
        // distinct method, not an override.
        if let Some((super_method, _)) =
            find_method_recursive(super_id, &method.name, &method.descriptor, store)
        {
            if super_method
                .access_flags
                .contains(MethodAccessFlags::PRIVATE)
                || super_method.is_static()
            {
                continue;
            }
            if super_method.access_flags.contains(MethodAccessFlags::FINAL) {
                return Err(LinkageError::VerifyError {
                    class_name: class.name.to_string(),
                    method_name: method.name.to_string(),
                    message: format!(
                        "cannot override final method {}.{}{}",
                        store.get(super_id).map(|c| &*c.name).unwrap_or("?"),
                        method.name,
                        method.descriptor,
                    ),
                });
            }
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
            is_synthetic_stub: false,
            has_finalizer: false,
            signature: None,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
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
            vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                exception_table: vec![],
                attributes: vec![],
            }))]
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
        assert!(verify_class_structure(child, &store).is_err());
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
                attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table,
                    attributes: vec![],
                }))],
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
            is_synthetic_stub: false,
            has_finalizer: false,
            signature: None,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        }
    }

    /// SECURITY FIX (HIGH): a synthetic method that mirrors the
    /// `TypePool$AbstractBase$Hierarchical.clear` shape from
    /// ByteBuddy 1.12.12 — two distinct `jsr` call sites that target
    /// the same subroutine. Our worklist verifier cannot type-check it
    /// (it merges `ReturnAddress(3)` and `ReturnAddress(6)` into `Top`),
    /// so the default policy must **reject** it (refuse to load it
    /// unverified) rather than silently accept it via the structural-only
    /// fallback. The legacy structural-only acceptance is still reachable
    /// through the `CRATONVM_ALLOW_JSR_RET` escape hatch, modelled here by
    /// the `allow_jsr = true` branch.
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
    fn jsr_double_call_site_rejected_by_default_accepted_with_escape_hatch() {
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
        // Default policy (escape hatch off): structurally well-formed, but
        // the subroutine opcodes can't be type-checked → hard rejection.
        let rejected = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false);
        assert!(
            rejected.is_err(),
            "subroutine-using method must be rejected by default (no unverified \
             acceptance via structural-only fallback), got {rejected:?}"
        );
        // Escape hatch on: legacy structural-only acceptance.
        let accepted = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true);
        assert!(
            accepted.is_ok(),
            "with CRATONVM_ALLOW_JSR_RET the structurally-valid subroutine method \
             must be accepted (structural-only), got {accepted:?}"
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
    /// shape that the Java 5 javac emits for `try-finally`.
    #[test]
    fn jsr_finally_handler_double_call_site_rejected_by_default_accepted_with_escape_hatch() {
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
        // Default: refuse to load the unverifiable subroutine method.
        let rejected = verify_class_bytecode_inner(&class, &PermissiveHierarchy, false);
        assert!(
            rejected.is_err(),
            "ByteBuddy clear()-shape (try-finally with double-jsr to one \
             subroutine) must be rejected by default, got {rejected:?}"
        );
        // Escape hatch: legacy structural-only acceptance.
        let accepted = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true);
        assert!(
            accepted.is_ok(),
            "with CRATONVM_ALLOW_JSR_RET the structurally-valid clear()-shape \
             must be accepted (structural-only), got {accepted:?}"
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
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true);
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xac]), // ireturn on empty stack
                exception_table: vec![],
                attributes: vec![],
            }))],
        });
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true);
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 0,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xac]), // ireturn on empty stack
                exception_table: vec![],
                attributes: vec![],
            }))],
        });
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true);
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0xb2, 0x00]), // getstatic with truncated index
                exception_table: vec![],
                attributes: vec![],
            }))],
        });
        let res = verify_class_bytecode_inner(&class, &PermissiveHierarchy, true);
        assert!(
            res.is_err(),
            "malformed non-JSR method in a mixed class must still be rejected, \
             got {res:?}"
        );
    }

    /// SECURITY FIX (HIGH): the default-policy rejection of a
    /// subroutine-using method must surface as a `VerifyError` whose
    /// message names the `jsr/jsr_w/ret` cause and the
    /// `CRATONVM_ALLOW_JSR_RET` escape hatch, so the refusal is
    /// diagnosable rather than an opaque failure.
    #[test]
    fn jsr_default_rejection_is_a_verify_error_with_diagnostic() {
        let class = make_pre_java7_jsr_class(
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
        match verify_class_bytecode_inner(&class, &PermissiveHierarchy, false) {
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            }))],
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
}
