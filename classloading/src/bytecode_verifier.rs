// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
use cratonvm_reader::verified_code::{decode_instructions, DecodedInstructions};

use super::class::Class;
use super::type_maps::{ClassTypeMaps, MethodTypeMaps, MethodTypeMapsBuilder};
use super::verify_frame::{HsType, PcBits, VerificationFrame};
use super::verify_hotspot_text::{self as hs, HsFrame, HsMethod, Reason};
use super::verify_old_text as old;
use super::verify_insn::{verify_instruction_with, VerifyScratch};
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
    verify_bytecode_inner(class, hierarchy, force_strict, None)
}

/// [`verify_bytecode`] / [`verify_bytecode_strict`] for a caller that has
/// already decoded every method of `class` (`decoded[i]` is the canonical
/// decode of `class.methods[i]`'s code, `None` where there is none or it
/// failed): the per-method decode lookups are not repeated. `explicit_strict`
/// is `-Xverify:all`.
pub(crate) fn verify_bytecode_decoded(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
    explicit_strict: bool,
    decoded: &mut [Option<DecodedInstructions>],
) -> Result<(), LinkageError> {
    let strict = explicit_strict || !class_is_bootstrap_trusted(class);
    verify_bytecode_inner(class, hierarchy, strict, Some(decoded))
}

/// The constant pool the verifier reads for `class`: a hidden class's own
/// `CONSTANT_Class` entries name the hidden class, as HotSpot's
/// `ClassFileParser` makes them (JIT round 21 wave 14, lane verifier).
///
/// A hidden class is stored under `<class-file name>/0x<hex>` and its constant
/// pool still carries the class-FILE name. HotSpot appends the mangled name as
/// a new `CONSTANT_Utf8` and points the `this_class` entry at it (measured for
/// lambda proxies, `vm/src/runtime/lambda_proxy.rs`), so every instruction and
/// every `StackMapTable` frame that names the class through that entry —
/// `this(...)` delegation (`aload_0; invokespecial <itself>.<init>`), a
/// `getfield` / `putfield` on `this`, a `new` / `checkcast` of itself — names
/// the very type the verifier gives `this`. Reading the unpatched pool made the
/// two names different types, so a delegating constructor failed with
/// "Bad <init> method call ... Type 'P' is not assignable to 'P/0x..'" where
/// HotSpot 25 runs the class (probe
/// `tools/probes/jit/r21/fuzz/R21Fz3FinHidden1.java`).
///
/// `Some(patched)` only for a hidden class one of whose `CONSTANT_Class`
/// entries names its class-file name; `None` means "read
/// `class.constant_pool`". The class store itself is untouched (the runtime
/// resolves the class-file name through `hidden_self_reference`). Field and
/// method DESCRIPTORS keep the class-file name, as in HotSpot (they are
/// separate `Utf8` entries). The class does not record its `this_class` index,
/// so every `CONSTANT_Class` entry naming the class-file name is re-pointed
/// (javac and the ClassFile API share one entry; a hand-written pool with a
/// second copy is the residual difference). `CRATONVM_VERIFY_HIDDEN_SELF_NAME=0`
/// restores the class-file name.
pub(crate) fn hidden_self_named_pool(class: &Class) -> Option<ConstantPool> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    if !class.is_hidden() {
        return None;
    }
    let cp = &class.constant_pool;
    let len = cp.len();
    // The new Utf8 is appended at index `len`, which must stay a `u16`.
    if len == 0 || len >= usize::from(u16::MAX) {
        return None;
    }
    let names_class = |name: &str| {
        (1..len).any(|i| match cp.get(i as u16) {
            Some(ConstantPoolEntry::ClassReference { name_index }) => {
                cp.get_utf8(*name_index) == Some(name)
            }
            _ => false,
        })
    };
    // The class-file name: the stored name with its `/0x<hex>` tail(s) cut
    // (a collision suffix may follow a pre-mangled name), longest first.
    let mut original: Option<&str> = None;
    let mut rest: &str = &class.name;
    while let Some((base, hex)) = rest.rsplit_once("/0x") {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            break;
        }
        if names_class(base) {
            original = Some(base);
            break;
        }
        rest = base;
    }
    let original = original?;
    if !true {
        return None;
    }
    let appended = len as u16;
    let mut entries: Vec<ConstantPoolEntry> = Vec::with_capacity(len + 1);
    let mut wide = std::collections::HashMap::new();
    for i in 0..len {
        let index = i as u16;
        let mut entry = cp
            .get(index)
            .cloned()
            .unwrap_or(ConstantPoolEntry::Tombstone);
        if let ConstantPoolEntry::ClassReference { name_index } = &mut entry {
            if cp.get_utf8(*name_index) == Some(original) {
                *name_index = appended;
            }
        }
        if let Some(units) = cp.get_utf8_wide(index) {
            wide.insert(index, Arc::<[u16]>::from(units));
        }
        entries.push(entry);
    }
    entries.push(ConstantPoolEntry::Utf8((&class.name).into()));
    Some(ConstantPool::new_with_wide(entries, wide))
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
    verify_bytecode_inner(class, hierarchy, true, None)
}

fn verify_bytecode_inner(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
    strict_verification: bool,
    decoded: Option<&mut [Option<DecodedInstructions>]>,
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
    //
    // zs s3/fp3: a class store keeps only each method's verdicts unless a
    // row reader is armed (`type_maps::rows_dropped_by_default`,
    // `CRATONVM_TYPE_MAP_DROP_ROWS=0` keeps the rows); the walk and the
    // verdicts are unchanged.
    // (A class with `jsr` / `ret` keeps the single walk: the split verifier
    // refuses those opcodes outright, so HotSpot's answer is libverify's
    // subroutine walk, `verifier.rs`'s route.)
    if class.version.major == ClassFileVersion::JAVA_6.major
        && strict_verification
        && crate::loader_flags().verify_major50_failover
        && !class.methods.iter().any(crate::verifier::method_uses_jsr_or_ret)
    {
        return verify_major_50_with_failover(class, hierarchy, strict_verification);
    }
    verify_bytecode_walk(class, hierarchy, strict_verification, decoded)
}

/// [`verify_bytecode_inner`]'s single walk over every method with the class's
/// own version (the type maps it publishes are this walk's).
fn verify_bytecode_walk(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
    strict_verification: bool,
    mut decoded: Option<&mut [Option<DecodedInstructions>]>,
) -> Result<(), LinkageError> {
    let mut collected = MethodSlots::new(class.methods.len());

    // One constant pool for the whole class: what the walk resolves from it
    // (field types, method signatures, array types) is resolved once.
    // The maps are published into `store_for(hierarchy)` below: verdicts only
    // when that store strips the rows anyway.
    let mut scratch = VerifyScratch::for_class_version(class.version.major)
        .for_store(crate::type_maps::store_for(hierarchy));
    let self_named = hidden_self_named_pool(class);
    let cp = self_named.as_ref().unwrap_or(&class.constant_pool);

    for method_index in method_visit_order(class) {
        let method = &class.methods[method_index];
        // Skip abstract and native methods — they have no Code attribute.
        // They still occupy an index in `Class::methods`, so a `None` entry
        // is filed to keep `method_index` aligned with the class's method
        // list (consumers index by that position).
        if method.is_abstract() || method.is_native() {
            collected.put(
                method_index,
                (method.name.clone(), method.descriptor.clone(), None),
            );
            continue;
        }

        let pre_decoded = decoded
            .as_deref_mut()
            .and_then(|d| d.get_mut(method_index))
            .and_then(|slot| slot.take());
        let maps = verify_method(
            &class.name,
            method,
            cp,
            &class.version,
            hierarchy,
            strict_verification,
            &mut scratch,
            pre_decoded,
            None,
        )
        .map_err(|e| major_50_split_message(class, method, e))?;
        collected.put(
            method_index,
            (method.name.clone(), method.descriptor.clone(), maps),
        );
    }

    // Publish only once every method verified: a class that fails
    // verification is never loaded, so half-built maps must not be visible.
    crate::type_maps::store_for(hierarchy)
        .publish(class.id, ClassTypeMaps::new(collected.into_vec()));

    Ok(())
}

/// The order in which a class's methods are verified: every `<init>`, then
/// every `<clinit>`, then the rest, each group in class-file order
/// (`CRATONVM_VERIFY_INITIALIZERS_FIRST`, HotSpot alignment, default ON; `=0`
/// walks class-file order as before; verifier-link-loads page, item 2).
///
/// HotSpot verifies `InstanceKlass::methods()`, which `Method::sort_methods`
/// orders by the ADDRESS of each name's `Symbol`. `<init>` and `<clinit>` are
/// `vmSymbols`, created first, and come first in every HotSpot 25 run
/// measured (Linux and Windows, CDS on and off). The rest is allocator
/// order, which HotSpot itself does not keep: with `-Xshare:off` a class's
/// new names move from last to first, and Windows orders them unlike Linux
/// (`VLoad.java`, `-Xlog:verification`). So only the
/// part every run agrees on is modelled; the order is visible as the order
/// of the verifier's `loadClass` calls on a user loader (the link replays
/// the walk's recorded loads) and as which method's `VerifyError` is reported
/// when several fail.
pub(crate) fn method_visit_order(class: &Class) -> Vec<usize> {
    let n = class.methods.len();
    if !crate::loader_flags().verify_initializers_first {
        return (0..n).collect();
    }
    let rank = |i: usize| match &*class.methods[i].name {
        "<init>" => 0u8,
        "<clinit>" => 1,
        _ => 2,
    };
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| rank(i)); // stable: class-file order within a rank
    order
}

/// One entry per method of a class, filed by method index in whatever order
/// [`method_visit_order`] walks them, read back in method-index order (what
/// [`ClassTypeMaps`] indexes by).
pub(crate) struct MethodSlots(Vec<Option<(Arc<str>, Arc<str>, Option<MethodTypeMaps>)>>);

impl MethodSlots {
    pub(crate) fn new(n: usize) -> Self {
        Self((0..n).map(|_| None).collect())
    }

    pub(crate) fn put(
        &mut self,
        index: usize,
        entry: (Arc<str>, Arc<str>, Option<MethodTypeMaps>),
    ) {
        self.0[index] = Some(entry);
    }

    pub(crate) fn into_vec(self) -> Vec<(Arc<str>, Arc<str>, Option<MethodTypeMaps>)> {
        self.0
            .into_iter()
            .map(|e| e.expect("method_visit_order visits every method once"))
            .collect()
    }
}

/// [`verify_method`] for one method of `class`, for its refusal only: the
/// subroutine-aware per-method route (`verifier::verify_class_bytecode_inner`
/// for a class with a `jsr` / `ret` method) walks the class's other methods
/// with its own copy of this walk and asks this one for HotSpot's text of a
/// refusal (interpreter round i5 wave 103, lane L7). No maps are published.
pub(crate) fn verify_method_for_refusal_text(
    class: &Class,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
    strict_verification: bool,
) -> Result<(), LinkageError> {
    let mut scratch = VerifyScratch::for_class_version(class.version.major);
    let self_named = hidden_self_named_pool(class);
    verify_method(
        &class.name,
        method,
        self_named.as_ref().unwrap_or(&class.constant_pool),
        &class.version,
        hierarchy,
        strict_verification,
        &mut scratch,
        None,
        None,
    )
    .map(|_| ())
    .map_err(|e| major_50_split_message(class, method, e))
}

/// Verify a single method's bytecode.
///
/// On success returns the [`MethodTypeMaps`] the verification walk produced —
/// the exact reference layout of every local slot and every operand-stack slot
/// at every instruction start, plus the per-method `safe_for_fast_path` proof.
/// `None` means the method has nothing to describe (no `Code` attribute, or an
/// empty one), which consumers must treat as "unproven".
#[allow(clippy::too_many_arguments)]
fn verify_method(
    class_name_arc: &Arc<str>,
    method: &ClassFileMethod,
    cp: &ConstantPool,
    version: &ClassFileVersion,
    hierarchy: &dyn ClassHierarchy,
    strict_verification: bool,
    scratch: &mut VerifyScratch,
    pre_decoded: Option<DecodedInstructions>,
    // Set only by [`pre_instruction_frame`]'s second walk: record the frame
    // an instruction started from (interpreter round i5 wave 101, lane L7).
    mut capture: Option<&mut FrameCapture>,
) -> Result<Option<MethodTypeMaps>, LinkageError> {
    let class_name: &str = class_name_arc;
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
    let mut structural_decode: Option<DecodedInstructions> = pre_decoded;
    // HotSpot's text for the fault (`hotspot_structural_refusal`; interpreter
    // round i5 wave 103, lane L7).
    crate::verifier::verify_method_structural_shared(class_name, method, &mut structural_decode)
        .map_err(|e| {
            crate::verifier::hotspot_structural_refusal(class_name, method, version.major)
                .unwrap_or(e)
        })?;

    // Establish the canonical decode and CFG contract before type-state
    // verification: the same decoder and validation the compilers use
    // (`verified_code` runs it too), so verifier and compiler cannot disagree
    // about instruction widths or branch boundaries. (`verify_method_structural`
    // made the decode and handed it back in `structural_decode`; it is not
    // entered in the compilers' process cache, see `decode_instructions`.)
    let verified = match structural_decode {
        Some(v) => v,
        None => decode_instructions(bytecode).map_err(|e| LinkageError::VerifyError {
            class_name: class_name.to_string(),
            method_name: method.name.to_string(),
            message: format!("failed to build verified code: {e}"),
        })?,
    };
    // TYPE MAPS lean on two guarantees this canonical decode establishes, so
    // they are asserted here rather than re-derived downstream:
    //   1. `code.len() <= 65535`, so every recorded pc fits a `u16` (the
    //      `u32` pc table (`TM_PC_WIDE`) is defence in depth, not a live path).
    //   2. every branch target is an instruction boundary, so no row in the
    //      pc table can describe a mid-instruction offset.
    // SBR round 5, lane warmup: the walk below reads instructions out of this
    // decode instead of re-running `Instruction::decode` on the same bytes
    // (`VerifiedCode` is built by that very decoder; `walk_insn` falls back to
    // `decode` for any pc that is not one of its instruction starts).
    let verified_insns = verified.instructions();
    let insn_count = verified_insns.len();

    // Find the StackMapTable attribute within the Code attribute
    let mut stack_map_table = find_stack_map_table(&code_attr.attributes);

    // Java 7+ (version >= 51) requires StackMapTable for verification
    let requires_stack_map = version.major >= ClassFileVersion::JAVA_7.major;

    // HotSpot's `StackMapReader` reads the whole table before the walk
    // (interpreter round i5 wave 109, lane L7). Below major 50 the class-file
    // parser does not even keep the attribute (libverify never reads it). At
    // major 50 a reader `VerifyError` / `class_format_error` fails over to
    // libverify, which ignores the table, so the walk goes on without it;
    // only a stream error (a pending `ClassFormatError`) stops the class.
    // The refusal itself is raised after the catch-type check below, in
    // HotSpot's order. Strict (untrusted) walks only.
    let mut stack_map_refusal: Option<StackMapRefusal> = None;
    // (`VerifyScratch::ignores_stack_map`: the libverify pass of a major-50
    // class's failover, wave 112.)
    if version.major < ClassFileVersion::JAVA_6.major || scratch.ignores_stack_map() {
        stack_map_table = None;
    } else if strict_verification {
        if let Some(raw) = stack_map_table {
            let mut initial_locals: Vec<u8> = Vec::new();
            if !method.is_static() {
                initial_locals.push(1);
            }
            for t in super::vtype::param_types_from_descriptor(&method.descriptor) {
                initial_locals.push(if t.is_category2() { 2 } else { 1 });
            }
            stack_map_refusal = stack_map_reader_refusal(
                raw,
                verified_insns,
                cp,
                &initial_locals,
                code_attr.max_locals,
                code_attr.max_stack,
            );
            if !requires_stack_map
                && !matches!(stack_map_refusal, None | Some(StackMapRefusal::Format(_)))
            {
                stack_map_refusal = None;
                stack_map_table = None;
            }
        }
    }

    // Below major 50 HotSpot reports libverify's texts (`verify_old_text`;
    // interpreter round i5 wave 102, lane L7).
    let old_texts = old::uses_old_texts(version.major);

    // HotSpot's VerifyError text for this method (`verify_hotspot_text`;
    // interpreter round i5 wave 101, lane L7). Built only on an error path.
    let hs_method = || HsMethod {
        class_name,
        method_name: &method.name,
        descriptor: &method.descriptor,
        code: bytecode,
        exception_table: &code_attr.exception_table,
        stack_map_raw: stack_map_table,
    };

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

    // HotSpot checks the handler table's catch types before the method's
    // code (`verify_exception_handler_table`; libverify's
    // `initialize_exception_table` before its dataflow), so a non-Throwable
    // catch type already loaded is refused here, ahead of any type error in
    // the code (interpreter round i5 wave 103, lane L7). One not loaded yet
    // is the link step's (`load_catch_types_at_link`).
    if let Some((entry, name)) =
        first_non_throwable_catch_type(&code_attr.exception_table, cp, class_name, method, hierarchy)
    {
        let message = if old_texts {
            old::message(
                class_name,
                &method.name,
                &method.descriptor,
                "catch_type not a subclass of Throwable",
            )
        } else {
            hs::render(
                &hs_method(),
                &format!(
                    "Catch type is not a subclass of Throwable in exception handler {}",
                    entry.handler_pc
                ),
                usize::from(entry.handler_pc),
                &Reason::WrongType {
                    actual: HsType::V(VType::ObjectRef(name)),
                    actual_origin: hs::Origin::Cp(entry.catch_type),
                    expected: HsType::V(VType::ObjectRef(Arc::from("java/lang/Throwable"))),
                    expected_origin: hs::Origin::Implicit,
                },
                None,
                None,
            )
        };
        return Err(LinkageError::VerifyError {
            class_name: class_name.to_string(),
            method_name: method.name.to_string(),
            message,
        });
    }

    if let Some(refusal) = stack_map_refusal {
        return Err(stack_map_refusal_error(
            refusal,
            &hs_method(),
            class_name_arc,
            method,
            cp,
            code_attr.max_locals,
            code_attr.max_stack,
        ));
    }

    if requires_stack_map && stack_map_table.is_none() {
        // No StackMapTable — only valid if there are no branches/exception handlers.
        // Check if the method has any branch targets that would require type checking.
        let has_exception_handlers = !code_attr.exception_table.is_empty();
        let has_branches = bytecode_has_branches(bytecode);
        if has_exception_handlers || has_branches {
            // HotSpot walks on and refuses the first jump (or handler) whose
            // target has no frame: "Expecting a stackmap frame at branch
            // target N" at that instruction.
            let message = match first_frameless_target(verified_insns, &code_attr.exception_table)
            {
                Some((at, target)) => hs::render(
                    &hs_method(),
                    &format!("Expecting a stackmap frame at branch target {target}"),
                    at,
                    &Reason::MissingStackmap,
                    None,
                    None,
                ),
                None => "Java 7+ method with branches/exception handlers requires StackMapTable"
                    .to_string(),
            };
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message,
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
            return verify_by_inference(
                class_name, method, code_attr, cp, hierarchy, scratch, old_texts,
            )
            .map(Some);
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

    // HotSpot reads the whole table before the walk; an `Uninitialized`
    // entry naming no `new` is a ClassFormatError there (interpreter round i5
    // wave 108, lane L7; it was accepted, and the `<init>` rules then had no
    // `new` site to check against). Only a class the split verifier decides
    // alone: a major-50 class fails over to libverify, which ignores the
    // table.
    if requires_stack_map && effective_strict {
        if let Some(table) = &parsed_table {
            if let Some(err) =
                uninitialized_offset_refusal(table, verified_insns, class_name, method)
            {
                return Err(err);
            }
        }
    }

    // Build the declared frames map: bytecode offset → VerificationFrame
    // StackMapTable frames are derived from a compact initial frame (method params only,
    // NOT padded to max_locals), per JVM spec 4.7.4.
    let (compact_frame, initial_frame) = VerificationFrame::initial_frames(
        class_name_arc,
        &method.name,
        &method.descriptor,
        method.is_static(),
        code_attr.max_locals,
        code_attr.max_stack,
    );

    let declared_frames = match &parsed_table {
        Some(table) => build_declared_frames(table, &compact_frame, cp, class_name, &method.name)?,
        // (an empty map allocates nothing; these used to be sized for 32 and 8
        // entries for EVERY method, a couple of KiB each, mostly one-line
        // getters with neither frames nor handlers)
        None => FxHashMap::default(),
    };

    // Build exception handler target set
    let mut handler_targets: FxHashMap<u16, VType> =
        FxHashMap::with_capacity_and_hasher(code_attr.exception_table.len(), Default::default());
    // Every handler with its own catch type, in table order, for HotSpot's
    // per-instruction handler-frame check (`verify_exception_handler_targets`;
    // interpreter round i5 wave 101, lane L7). `handler_targets` keeps one
    // type per handler pc; this keeps each entry's.
    let mut handlers: Vec<(u16, u16, u16, VType)> = Vec::new();
    for entry in &code_attr.exception_table {
        let catch_type = catch_type_of(entry, cp, class_name, &method.name)?;
        // The catch-type loads HotSpot's `verify_exception_handler_table` makes
        // before the method's code, in that order, as `verifier::verify_method_typestate`
        // records them -- for a hierarchy that records loads (the link's single
        // walk, `CRATONVM_USER_PASS3_RECORDS_LINK_LOADS`; interpreter round i6
        // wave 119, lane L5). Not on the capture walk (it repeats a method).
        if entry.catch_type != 0 && capture.is_none() && hierarchy.records_verifier_loads() {
            let throwable =
                VType::ObjectRef(crate::vtype::with_well_known(|w| Arc::clone(&w.throwable)));
            if catch_type != throwable {
                crate::vtype::note_hotspot_assignability_loads(&catch_type, &throwable, hierarchy);
            }
        }
        handlers.push((entry.start_pc, entry.end_pc, entry.handler_pc, catch_type.clone()));
        handler_targets.insert(entry.handler_pc, catch_type);
    }
    // Each handler's catch-type-to-frame-slot verdict, asked once per method
    // (`CRATONVM_VERIFY_HANDLER_CATCH_MEMO`, default ON): see
    // [`check_handler_frames_at_memo`]. Empty (no memo) when off.
    let handler_catch_memo: Vec<std::cell::Cell<u8>> =
        if crate::loader_flags().verify_handler_catch_memo {
            vec![std::cell::Cell::new(CATCH_MEMO_UNKNOWN); handlers.len()]
        } else {
            Vec::new()
        };
    // The handler ranges' union bounds (HotSpot's `ex_min` / `ex_max`).
    let ex_min = handlers.iter().map(|h| usize::from(h.0)).min().unwrap_or(0);
    let ex_max = handlers.iter().map(|h| usize::from(h.1)).max().unwrap_or(0);

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
        return verify_by_inference(
            class_name, method, code_attr, cp, hierarchy, scratch, old_texts,
        )
        .map(Some);
    }

    // TYPE MAPS: ride the walk below. `record` is called once per instruction
    // start with the type-state that holds immediately BEFORE that
    // instruction executes — after any declared StackMapTable frame or
    // exception-handler frame has been adopted, so the recorded state is the
    // one the interpreter/GC would actually observe at that pc.
    // JIT round 23 wave 5 (`CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE`): the
    // previous method's emptied row vectors, when the switch is on.
    let reuse_type_map_buffers = crate::loader_flags().verify_type_map_buffer_reuse;
    let mut type_maps = if reuse_type_map_buffers {
        MethodTypeMapsBuilder::with_spare(
            code_attr.max_locals,
            code_attr.max_stack,
            scratch.type_map_spare_mut(),
        )
    } else {
        MethodTypeMapsBuilder::new(code_attr.max_locals, code_attr.max_stack)
    };
    if scratch.verdict_only_type_maps() {
        type_maps.set_verdict_only();
    }
    // Exact, not estimated: `verified_code` already counted the instructions.
    type_maps.reserve(insn_count);
    // Cleared whenever the walk skips or truncates a region, which denies
    // `safe_for_fast_path` (the unchecked interpreter handlers need every
    // executed instruction proven, not merely most of them).
    let mut walk_complete = true;

    // HotSpot's handler-frame check applies to strictly verified Java 7+
    // methods that declare frames (`CRATONVM_VERIFY_HANDLER_FRAMES`, default
    // on, `=0` off; interpreter round i5 wave 101, lane L7).
    let check_handler_frames = !handlers.is_empty()
        && requires_stack_map
        && effective_strict
        && parsed_table.is_some()
        && crate::loader_flags().verify_handler_frames;

    // Cursor into `verified_insns` (pcs only increase through the walk).
    let mut insn_ix = 0usize;
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

    // Offsets that carry a declared frame or are a handler entry: the only pcs
    // at which the two maps below can say anything, so the other
    // (overwhelmingly common) pcs skip four hash probes each.
    let mut special_pcs = PcBits::new(bytecode.len());
    for &offset in declared_frames.keys() {
        special_pcs.insert(offset as usize);
    }
    for &offset in handler_targets.keys() {
        special_pcs.insert(offset as usize);
    }

    // The last instruction walked falls through past the code's end
    // (interpreter round i5 wave 102, lane L7). Only a REACHABLE one counts:
    // `live` is false in code no edge reaches (dead code after a `return`
    // that this walk still visits below major 51, and HotSpot never does).
    let mut falls_off_end = false;
    let mut live = true;
    // HotSpot's `flagThisUninit` of the current frame (interpreter round i5
    // wave 108, lane L7): set on a constructor's entry frame, taken from each
    // declared frame the walk adopts, cleared only by the `<init>` call on
    // `uninitializedThis` (overwriting the local does not clear it).
    let flag_rules =
        requires_stack_map && effective_strict && crate::loader_flags().verify_this_uninit_flags;
    let mut this_uninit = locals_hold_uninit_this(&current_frame);
    // A major-50 class refused by both verifiers reports the SPLIT verifier's
    // message (`Verifier::verify`). In a straight-line method without a
    // table (no branch, no handler: the split verifier needs no frame there)
    // its first refusal is the instruction this walk refuses, so the fault is
    // rendered as the split verifier's (interpreter round i5 wave 109, lane
    // L7; `i109-L7-major-50-refusals-are-worded-as-our-own`). A method with
    // branches keeps this VM's text: the split verifier may first refuse a
    // missing frame there.
    let split_texts = requires_stack_map
        || (version.major == ClassFileVersion::JAVA_6.major
            && stack_map_table.is_none()
            && code_attr.exception_table.is_empty()
            && !bytecode_has_branches(bytecode));
    // Below major 51 the receiver rule applies to a straight-line method
    // only (no merge, so no coarse type), as libverify's
    // "Incompatible object argument for function call" (interpreter round i5
    // wave 109, lane L7).
    // (Wave 110: not when a table is walked; a declared frame can be coarser
    // than libverify's own merge, which ignores the table.)
    if !requires_stack_map
        && stack_map_table.is_none()
        && code_attr.exception_table.is_empty()
        && !bytecode_has_branches(bytecode)
    {
        scratch.apply_old_receiver_rule(&method.name, &method.descriptor, false);
    }
    while pc < bytecode.len() {
        let is_special = special_pcs.contains(pc);
        live = is_special || (live && verified);
        // Check if this PC is a declared frame target
        if let Some(declared) = declared_frames.get(&(pc as u16)).filter(|_| is_special) {
            // At a merge point: verify current frame is assignable to declared frame
            if verified && !current_frame.is_assignable_to(declared, hierarchy) {
                let mismatch = hs::frame_mismatch(&current_frame, declared, hierarchy)
                    .filter(|_| requires_stack_map);
                let message = match mismatch {
                    Some(mismatch) => hs::render_mismatch(
                        &hs_method(),
                        "Instruction type does not match stack map",
                        pc,
                        pc,
                        mismatch,
                        &HsFrame::current(&current_frame, pc),
                        &current_frame,
                        declared,
                    ),
                    None => format!(
                        "frame mismatch at bytecode offset {pc}: \
                         current frame is not assignable to declared StackMapTable frame"
                    ),
                };
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message,
                });
            }
            let declared_flag = locals_hold_uninit_this(declared);
            if flag_rules && verified && this_uninit && !declared_flag {
                let mut printed = HsFrame::current(&current_frame, pc);
                printed.this_uninit = true;
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: hs::render_flags_mismatch(
                        &hs_method(),
                        "Instruction type does not match stack map",
                        pc,
                        &printed,
                        declared,
                    ),
                });
            }
            this_uninit = declared_flag;
            // Use the declared frame going forward (narrows types).
            // Pad to max_locals since StackMapTable frames may be compact.
            current_frame.adopt_declared(declared, code_attr.max_locals);
            // TYPE MAPS: a declared StackMapTable frame IS the authoritative
            // merge-point state for this pc — every edge into it was (or, at
            // the branch sites below, will be) checked against it.
            authoritative = true;
        }

        // Check if this PC is an exception handler entry
        if let Some(catch_type) = handler_targets.get(&(pc as u16)).filter(|_| is_special) {
            if !verified {
                // This is an exception handler entry point — start with the handler frame
                // (the current frame itself: the frame is discarded with the
                // method on the error path, so it is edited in place).
                current_frame.clear_stack();
                current_frame
                    .push(catch_type.clone())
                    .map_err(|_| LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("exception handler stack overflow at offset {pc}"),
                    })?;
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
        if !verified && requires_stack_map && !is_special {
            // Strict mode: reject outright. Lenient mode: skip the
            // unreachable code silently (some older compilers emit
            // dead code after branches; rejecting them would break
            // backward compatibility).
            if effective_strict {
                // HotSpot (`verify_stackmap_table`): "Expecting a stack map
                // frame" at the first pc control cannot reach, with
                // `missing_stackmap(bci)` while a later frame is declared and
                // `bad_code(bci)` past the last one.
                let reason = if declared_frames.keys().any(|&o| usize::from(o) > pc) {
                    Reason::MissingStackmap
                } else {
                    Reason::BadCode
                };
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: hs::render(
                        &hs_method(),
                        "Expecting a stack map frame",
                        pc,
                        &reason,
                        None,
                        None,
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

        // The instruction at `pc`: from the canonical decode when `pc` is one of
        // its instruction starts (every pc the walk reaches is), else decoded
        // here as before.
        while insn_ix < verified_insns.len() && (verified_insns[insn_ix].pc as usize) < pc {
            insn_ix += 1;
        }
        let decoded_here: Instruction;
        let (insn, next_pc): (&Instruction, usize) = match verified_insns.get(insn_ix) {
            Some(v) if v.pc as usize == pc => (&v.instruction, v.next_pc as usize),
            _ => match Instruction::decode(bytecode, pc) {
                Ok((decoded, next_pc)) => {
                    decoded_here = decoded;
                    (&decoded_here, next_pc)
                }
                Err(e) => {
                    return Err(LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: format!("failed to decode instruction at offset {pc}: {e}"),
                    });
                }
            },
        };

        // TYPE MAPS: fold this instruction into the per-method fast-path
        // safety proof (local-slot operand bounds, jsr/ret, stray `wide`).
        type_maps.observe_instruction(insn);

        // The second walk of `pre_instruction_frame`: keep the frame this
        // instruction starts from.
        if let Some(c) = capture.as_deref_mut() {
            if c.pc == pc && c.frame.is_none() {
                c.frame = Some(current_frame.clone());
            }
        }

        // HotSpot checks a store into a local against its handlers with the
        // INCOMING locals (before the store adds one), every other
        // instruction after it ran.
        let in_handler_range = check_handler_frames && pc >= ex_min && pc < ex_max;
        let is_local_store = matches!(
            insn,
            Instruction::Istore(_)
                | Instruction::Lstore(_)
                | Instruction::Fstore(_)
                | Instruction::Dstore(_)
                | Instruction::Astore(_)
        );
        if in_handler_range && is_local_store {
            check_handler_frames_at_memo(
                &hs_method(),
                pc,
                &current_frame,
                flag_rules.then_some(this_uninit),
                &handlers,
                &declared_frames,
                hierarchy,
                &handler_catch_memo,
            )
            .map_err(|message| LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message,
            })?;
        }

        // HotSpot's `return` in a constructor checks `flagThisUninit`, not
        // the locals: a constructor that overwrote `this` before `super()`
        // is still refused (interpreter round i5 wave 108, lane L7; the
        // instruction's own check sees only the locals and the stack).
        if flag_rules
            && this_uninit
            && matches!(insn, Instruction::Return)
            && !current_frame.has_uninitialized_this()
        {
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: hs::render(
                    &hs_method(),
                    "Constructor must call super() or this() before return",
                    pc,
                    &Reason::BadCode,
                    None,
                    None,
                ),
            });
        }
        // An `invokespecial <init>` in a `try` range: HotSpot's
        // `verify_invoke_init` checks the handlers with the INCOMING locals
        // (before `initialize_object`), the flag set for `uninitializedThis`.
        let init_receiver = if flag_rules {
            init_call_receiver(insn, &current_frame, cp)
        } else {
            None
        };
        let init_this_call = matches!(init_receiver, Some(VType::UninitializedThis));
        let pre_init_frame = if in_handler_range && init_receiver.is_some() {
            Some(current_frame.clone())
        } else {
            None
        };

        // Verify the instruction's type effects
        let result = match verify_instruction_with(
            insn,
            pc,
            &mut current_frame,
            cp,
            class_name,
            &method.name,
            &method.descriptor,
            hierarchy,
            scratch,
        ) {
            Ok(result) => result,
            Err(e) => {
                // HotSpot's text when a frame operation recorded what it
                // found and this is the first walk (the second walk only
                // fetches the starting frame, so it reports nothing).
                let fault = current_frame.take_fault();
                let hotspot = match (fault, capture.is_none() && split_texts) {
                    (Some(fault), true) => pre_instruction_frame(
                        class_name_arc,
                        method,
                        cp,
                        version,
                        hierarchy,
                        strict_verification,
                        scratch,
                        pc,
                    )
                    .map(|pre| {
                        hs::render_fault(&hs_method(), pc, &fault, &HsFrame::current(&pre, pc))
                    }),
                    // libverify's text below major 50 (interpreter round i5
                    // wave 102, lane L7), replayed from the starting frame.
                    (fault, false) if capture.is_none() && old_texts => pre_instruction_frame(
                        class_name_arc,
                        method,
                        cp,
                        version,
                        hierarchy,
                        strict_verification,
                        scratch,
                        pc,
                    )
                    .and_then(|pre| {
                        old::instruction_reason(&old::OldCtx {
                            insn,
                            pre: &pre,
                            fault: fault.as_ref(),
                            cp,
                            class_name,
                            method_name: &method.name,
                            descriptor: &method.descriptor,
                            is_static: method.is_static(),
                        })
                    })
                    .map(|reason| {
                        old::message(class_name, &method.name, &method.descriptor, &reason)
                    }),
                    _ => None,
                };
                // Add context to verify errors
                return Err(match e {
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
                        message: hotspot
                            .unwrap_or_else(|| format!("at bytecode offset {pc}: {message}")),
                    },
                    other => other,
                });
            }
        };

        if let Some(pre) = &pre_init_frame {
            check_handler_frames_at_memo(
                &hs_method(),
                pc,
                pre,
                Some(this_uninit || init_this_call),
                &handlers,
                &declared_frames,
                hierarchy,
                &handler_catch_memo,
            )
            .map_err(|message| LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message,
            })?;
        }
        // HotSpot's `this_uninit` for the handler check after the
        // instruction stays set by the `<init>` call; the frame's flag is
        // cleared (`initialize_object`).
        let handler_flag = this_uninit || init_this_call;
        if init_this_call {
            this_uninit = false;
        }

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
                // HotSpot: `ErrorContext::missing_stackmap(bci)` at the jump.
                return Err(LinkageError::VerifyError {
                    class_name: class_name.to_string(),
                    method_name: method.name.to_string(),
                    message: hs::render(
                        &hs_method(),
                        &format!("Expecting a stackmap frame at branch target {target}"),
                        pc,
                        &Reason::MissingStackmap,
                        None,
                        None,
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
                return verify_by_inference(
                    class_name, method, code_attr, cp, hierarchy, scratch, old_texts,
                )
                .map(Some);
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
                    // HotSpot's `check_jump_target` text; the current frame
                    // it prints is the one the jump started from.
                    let hotspot = match hs::frame_mismatch(&current_frame, target_frame, hierarchy)
                    {
                        Some(mismatch) if capture.is_none() && requires_stack_map => {
                            pre_instruction_frame(
                                class_name_arc,
                                method,
                                cp,
                                version,
                                hierarchy,
                                strict_verification,
                                scratch,
                                pc,
                            )
                            .map(|pre| {
                                hs::render_mismatch(
                                    &hs_method(),
                                    &format!(
                                        "Inconsistent stackmap frames at branch target {target}"
                                    ),
                                    pc,
                                    usize::from(target),
                                    mismatch,
                                    &HsFrame::current(&pre, pc),
                                    &current_frame,
                                    target_frame,
                                )
                            })
                        }
                        _ => None,
                    };
                    return Err(LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: hotspot.unwrap_or_else(|| {
                            format!(
                                "frame mismatch on branch to offset {target}: \
                                 current frame is not assignable to the declared \
                                 StackMapTable frame at the branch target"
                            )
                        }),
                    });
                }
                // `check_jump_target`'s flag comparison (interpreter round i5
                // wave 108, lane L7): a branch out of a constructor before
                // `super()` into a frame without `uninitializedThis` would
                // otherwise reach a `return` with the flag lost.
                if flag_rules && this_uninit && !locals_hold_uninit_this(target_frame) {
                    let pre = if capture.is_none() {
                        pre_instruction_frame(
                            class_name_arc,
                            method,
                            cp,
                            version,
                            hierarchy,
                            strict_verification,
                            scratch,
                            pc,
                        )
                    } else {
                        None
                    };
                    let mut printed = match &pre {
                        Some(pre) => HsFrame::current(pre, pc),
                        None => HsFrame::current(&current_frame, pc),
                    };
                    printed.this_uninit = true;
                    return Err(LinkageError::VerifyError {
                        class_name: class_name.to_string(),
                        method_name: method.name.to_string(),
                        message: hs::render_flags_mismatch(
                            &hs_method(),
                            &format!("Inconsistent stackmap frames at branch target {target}"),
                            usize::from(target),
                            &printed,
                            target_frame,
                        ),
                    });
                }
            }
        }

        if in_handler_range && !is_local_store {
            check_handler_frames_at_memo(
                &hs_method(),
                pc,
                &current_frame,
                flag_rules.then_some(handler_flag),
                &handlers,
                &declared_frames,
                hierarchy,
                &handler_catch_memo,
            )
            .map_err(|message| LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message,
            })?;
        }

        verified = result.falls_through;
        falls_off_end = live && result.falls_through && next_pc >= bytecode.len();

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

    // HotSpot: control may not fall through the end of the code
    // (`CRATONVM_VERIFY_CONTROL_RULES`, default on; it was accepted, and the
    // interpreter ran off the code array's end).
    if falls_off_end && crate::loader_flags().verify_control_rules {
        return Err(falls_off_end_error(&hs_method(), class_name, method, old_texts));
    }

    if reuse_type_map_buffers {
        return Ok(Some(
            type_maps.finish_into_spare(walk_complete, scratch.type_map_spare_mut()),
        ));
    }
    Ok(Some(type_maps.finish(walk_complete)))
}

/// The refusal of a method whose last instruction falls through the end of
/// its code: HotSpot's split verifier says "Control flow falls through code
/// end" (`bad_code(code_length)`), libverify (below major 50) "Falling off
/// the end of the code". Interpreter round i5 wave 102, lane L7.
#[cold]
fn falls_off_end_error(
    m: &HsMethod<'_>,
    class_name: &str,
    method: &ClassFileMethod,
    old_texts: bool,
) -> LinkageError {
    let message = if old_texts {
        old::message(
            class_name,
            &method.name,
            &method.descriptor,
            "Falling off the end of the code",
        )
    } else {
        hs::render(
            m,
            "Control flow falls through code end",
            m.code.len(),
            &Reason::BadCode,
            None,
            None,
        )
    };
    LinkageError::VerifyError {
        class_name: class_name.to_string(),
        method_name: method.name.to_string(),
        message,
    }
}

/// HotSpot's `flagThisUninit` of a DECLARED frame: `StackMapReader` sets it
/// when a local is `uninitializedThis` (a `full_frame` from its own locals, an
/// `append_frame` from the previous flags and its new locals, a `chop_frame`
/// recomputed from the locals that remain, the `same*` frames inherited from
/// the previous frame, whose locals they keep), so it is exactly "a declared
/// local is `uninitializedThis`". Interpreter round i5 wave 108, lane L7.
fn locals_hold_uninit_this(frame: &VerificationFrame) -> bool {
    frame.locals.iter().any(|t| *t == VType::UninitializedThis)
}

/// The receiver an `invokespecial <init>` at the top of `frame` would
/// consume (`None` for any other instruction or a stack too shallow).
fn init_call_receiver(
    insn: &Instruction,
    frame: &VerificationFrame,
    cp: &ConstantPool,
) -> Option<VType> {
    let Instruction::Invokespecial(index) = insn else {
        return None;
    };
    let (_, name, descriptor) = super::verify_insn::member_ref_parts(cp, *index)?;
    if name != "<init>" {
        return None;
    }
    let slots: usize = super::vtype::param_types_from_descriptor(descriptor)
        .iter()
        .map(|t| if t.is_category2() { 2 } else { 1 })
        .sum();
    let depth = frame.stack.len();
    depth
        .checked_sub(slots + 1)
        .and_then(|i| frame.stack.get(i))
        .cloned()
}

/// `inner` for the libverify-rules inference walk (interpreter round i5
/// waves 111-112, lane L7).
///
/// * A common superclass that is `java/lang/Object` because either class is
///   not loaded (`is_resolvable` false: exactly
///   `ClassManager::common_superclass`'s fallback to its static table) is
///   handed out as a NAMED `Arc` kept in `fallbacks`; so is any merge one of
///   whose inputs is such an `Arc` (by pointer). The receiver rule accepts
///   exactly those values ([`ClassHierarchy::is_fallback_merge`]); every
///   other merged type is libverify's exact answer.
/// * Arrays merge as `check_code.c` `merge_fullinfo_types` does
///   ([`Self::libverify_array_merge`]).
/// * Every other question is delegated unchanged (the verifier-load records
///   included).
struct MergeFallbackWatch<'a> {
    inner: &'a dyn ClassHierarchy,
    fallbacks: std::cell::RefCell<Vec<Arc<str>>>,
}

impl MergeFallbackWatch<'_> {
    /// Is `s` (by address) one of the fallback names handed out?
    fn is_fallback_str(&self, s: &str) -> bool {
        self.fallbacks
            .borrow()
            .iter()
            .any(|f| std::ptr::eq(f.as_ptr(), s.as_ptr()) && f.len() == s.len())
    }

    /// A new fallback-marked name.
    fn fallback(&self, name: &str) -> Arc<str> {
        let named: Arc<str> = Arc::from(name);
        self.fallbacks.borrow_mut().push(Arc::clone(&named));
        named
    }

    /// `merge_fullinfo_types` for two references at least one of which is
    /// an array (neither `null`): `java/lang/Object` on either side wins; an
    /// array against `java/lang/Cloneable` / `java/io/Serializable` is that
    /// interface; a primitive-element array is promoted to an `Object`
    /// array one dimension shorter (`[I` -> `Object`, `[[I` -> `[Object`);
    /// equal dimensions merge their element classes; unequal ones give the
    /// smaller dimension's value when its element is `Cloneable` /
    /// `Serializable`, else an `Object` array of the smaller dimension.
    fn libverify_array_merge(&self, a: &VType, b: &VType) -> Option<VType> {
        let name = |v: &VType| -> Option<Arc<str>> {
            match v {
                VType::ObjectRef(n) | VType::ArrayRef(n) => Some(Arc::clone(n)),
                _ => None,
            }
        };
        let (na, nb) = (name(a)?, name(b)?);
        if !matches!(a, VType::ArrayRef(_)) && !matches!(b, VType::ArrayRef(_)) {
            return None;
        }
        let inherited = self.is_fallback_str(&na) || self.is_fallback_str(&nb);
        const OBJECT: &str = "java/lang/Object";
        let object = |fell: bool| {
            VType::ObjectRef(if fell { self.fallback(OBJECT) } else { Arc::from(OBJECT) })
        };
        if &*na == OBJECT || &*nb == OBJECT {
            return Some(object(inherited));
        }
        let special = |n: &str| n == "java/lang/Cloneable" || n == "java/io/Serializable";
        if matches!(b, VType::ObjectRef(_)) && special(&*nb) {
            return Some(b.clone());
        }
        if matches!(a, VType::ObjectRef(_)) && special(&*na) {
            return Some(a.clone());
        }
        // (dimensions, element class) after the primitive promotion.
        let parts = |v: &VType, n: &str| -> (usize, String) {
            if matches!(v, VType::ObjectRef(_)) {
                return (0, n.to_string());
            }
            let dims = n.bytes().take_while(|&c| c == b'[').count();
            let rest = &n[dims..];
            match rest.strip_prefix('L').and_then(|r| r.strip_suffix(';')) {
                Some(class) => (dims, class.to_string()),
                None => (dims.saturating_sub(1), OBJECT.to_string()),
            }
        };
        let (da, ba) = parts(a, &*na);
        let (db, bb) = parts(b, &*nb);
        let build = |dims: usize, base: &str, fell: bool| -> VType {
            if dims == 0 {
                return VType::ObjectRef(if fell { self.fallback(base) } else { Arc::from(base) });
            }
            let desc = format!("{}L{base};", "[".repeat(dims));
            VType::ArrayRef(if fell { self.fallback(&desc) } else { Arc::from(desc.as_str()) })
        };
        if da == db {
            if ba == bb {
                return Some(build(da, ba.as_str(), inherited));
            }
            if ba == OBJECT || bb == OBJECT {
                return Some(build(da, OBJECT, inherited));
            }
            let common = self.merged_class_name(&ba, &bb);
            let fell = inherited || self.is_fallback_str(&common);
            return Some(build(da, &*common, fell));
        }
        let (small, small_dims, small_base) = if da < db { (a, da, &ba) } else { (b, db, &bb) };
        if special(small_base.as_str()) && small_dims > 0 {
            return Some(small.clone());
        }
        Some(build(small_dims, OBJECT, inherited))
    }
}

impl ClassHierarchy for MergeFallbackWatch<'_> {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        self.inner.is_subclass(child, parent)
    }

    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        self.inner.is_direct_superclass(child, parent)
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        self.inner.common_superclass(a, b)
    }

    fn merged_class_name(&self, a: &str, b: &str) -> Arc<str> {
        let inherited = self.is_fallback_str(a) || self.is_fallback_str(b);
        let common = self.inner.merged_class_name(a, b);
        let fell_back = &*common == "java/lang/Object"
            && (!self.inner.is_resolvable(a) || !self.inner.is_resolvable(b));
        if inherited || fell_back {
            return self.fallback(&common);
        }
        common
    }

    fn merge_references(&self, a: &VType, b: &VType) -> Option<VType> {
        self.libverify_array_merge(a, b)
    }

    fn is_fallback_merge(&self, name: &Arc<str>) -> bool {
        self.is_fallback_str(name)
    }

    fn is_interface(&self, name: &str) -> bool {
        self.inner.is_interface(name)
    }

    fn is_resolvable(&self, name: &str) -> bool {
        self.inner.is_resolvable(name)
    }

    fn type_map_store(&self) -> Option<&crate::type_maps::TypeMapStore> {
        self.inner.type_map_store()
    }

    fn records_verifier_loads(&self) -> bool {
        self.inner.records_verifier_loads()
    }

    fn note_verifier_load(&self, name: &str) {
        self.inner.note_verifier_load(name)
    }

    fn note_verifier_load_unless_interface(&self, target: &str, source: &str) {
        self.inner.note_verifier_load_unless_interface(target, source)
    }

    fn invokespecial_target(
        &self,
        current: &str,
        ref_class: &str,
    ) -> Option<super::vtype::SpecialTarget> {
        self.inner.invokespecial_target(current, ref_class)
    }

    fn catch_type_is_throwable(&self, name: &str) -> Option<bool> {
        self.inner.catch_type_is_throwable(name)
    }

    fn protected_receiver_rule(
        &self,
        current: &str,
        ref_class: &str,
        name: &str,
        descriptor: &str,
        is_method: bool,
    ) -> bool {
        self.inner
            .protected_receiver_rule(current, ref_class, name, descriptor, is_method)
    }
}

/// HotSpot's `Verifier::verify` for a major-50 class (interpreter round i5
/// wave 112, lane L7; `CRATONVM_VERIFY_MAJOR50_FAILOVER`, default on):
///
/// 1. The split verifier: every method with the major-51 rules and its
///    StackMapTable (a method without one refuses at its first branch, as the
///    split verifier does). All accepted: the class verifies, with these
///    maps; libverify never runs.
/// 2. A refusal that is a PENDING exception (`StackMapStream`'s
///    "StackMapTable format error: <text>" with no method suffix) is thrown
///    as is: `Verifier::verify` fails over only when none is pending.
/// 3. Otherwise libverify (`FailOverToOldVerifier`): every method with the
///    major-50 rules and no table. All accepted: the class verifies.
///    Refused: the class fails with the SPLIT verifier's message
///    (`exception_message = split_verifier.exception_message()`, measured on
///    w109: `nonvirtual-50`, `frameless-50-type-error`).
///
/// The split pass stops at its first refusing method in class-file order;
/// HotSpot walks `InstanceKlass::methods()`, sorted by name Symbol address,
/// so with two refusing methods the one reported may differ.
fn verify_major_50_with_failover(
    class: &Class,
    hierarchy: &dyn ClassHierarchy,
    strict_verification: bool,
) -> Result<(), LinkageError> {
    let self_named = hidden_self_named_pool(class);
    let cp = self_named.as_ref().unwrap_or(&class.constant_pool);
    let walk = |version: &ClassFileVersion,
                scratch: &mut VerifyScratch|
     -> Result<Vec<(Arc<str>, Arc<str>, Option<MethodTypeMaps>)>, LinkageError> {
        let mut collected = MethodSlots::new(class.methods.len());
        for method_index in method_visit_order(class) {
            let method = &class.methods[method_index];
            if method.is_abstract() || method.is_native() {
                collected.put(
                    method_index,
                    (method.name.clone(), method.descriptor.clone(), None),
                );
                continue;
            }
            let maps = verify_method(
                &class.name,
                method,
                cp,
                version,
                hierarchy,
                strict_verification,
                scratch,
                None,
                None,
            )?;
            collected.put(
                method_index,
                (method.name.clone(), method.descriptor.clone(), maps),
            );
        }
        Ok(collected.into_vec())
    };
    let mut split_scratch = VerifyScratch::for_class_version(ClassFileVersion::JAVA_7.major);
    let split_error = match walk(&ClassFileVersion::JAVA_7, &mut split_scratch) {
        Ok(collected) => {
            crate::type_maps::store_for(hierarchy)
                .publish(class.id, ClassTypeMaps::new(collected));
            return Ok(());
        }
        Err(e) => e,
    };
    if let LinkageError::ClassFormatError { message, .. } = &split_error {
        if message.starts_with("StackMapTable format error: ") && !message.contains(" in method '")
        {
            return Err(split_error);
        }
    }
    let mut old_scratch =
        VerifyScratch::for_class_version(class.version.major).ignoring_stack_maps();
    match walk(&class.version, &mut old_scratch) {
        // libverify accepts: the class verifies. Its type maps come from the
        // class's usual single walk (which also uses a declared table), or
        // from this walk when that one refuses.
        Ok(collected) => match verify_bytecode_walk(class, hierarchy, strict_verification, None) {
            Ok(()) => Ok(()),
            Err(_) => {
                crate::type_maps::store_for(hierarchy)
                    .publish(class.id, ClassTypeMaps::new(collected));
                Ok(())
            }
        },
        Err(_) => Err(split_error),
    }
}

/// HotSpot's message for a refusal `e` of `method` in a major-50 class
/// (interpreter round i5 wave 110, lane L7;
/// `i109-L7-major-50-refusals-are-worded-as-our-own`). The split verifier ran
/// first; this walk (libverify's rules) refusing means the failover refused
/// too, so `Verifier::verify` reports the SPLIT verifier's first refusal. A
/// refusal already in HotSpot's shape (split `Exception Details`, a
/// `ClassFormatError`) is kept; one in this VM's words ("at bytecode offset
/// N: ...") is replaced by `verifier::split_refusal_of_frameless_method`
/// when that can name the split verifier's refusal for certain (a method
/// without a table whose first split refusal, a missing frame at its first
/// branch or handler-covered instruction, lies at or before N, and no other
/// method of the class the split verifier would refuse).
fn major_50_split_message(class: &Class, method: &ClassFileMethod, e: LinkageError) -> LinkageError {
    if class.version.major != ClassFileVersion::JAVA_6.major {
        return e;
    }
    let pc = match &e {
        LinkageError::VerifyError { message, .. } if !message.contains(hs::DETAILS_MARKER) => message
            .strip_prefix("at bytecode offset ")
            .and_then(|rest| rest.split_once(':'))
            .and_then(|(n, _)| n.parse::<usize>().ok()),
        _ => None,
    };
    match pc.and_then(|pc| crate::verifier::split_refusal_of_frameless_method(class, method, pc)) {
        Some(split) => split,
        None => e,
    }
}

/// The first refusal HotSpot's `StackMapReader` / `StackMapTable`
/// constructor meets reading a method's raw `StackMapTable` (interpreter
/// round i5 wave 109, lane L7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StackMapRefusal {
    /// `StackMapStream::stackmap_format_error` (a pending `ClassFormatError`
    /// "StackMapTable format error: <text>", no method suffix): "access
    /// beyond the end of attribute", "reserved frame type", "bad verification
    /// type", "bad class index", "wrong attribute size".
    Format(&'static str),
    /// `class_format_error` "StackMapTable format error: bad offset for
    /// Uninitialized" (with " in method '<external name>'").
    UninitOffset,
    /// `class_format_error` "StackMapTable format error: bad type array
    /// size" (with " in method '<external name>'"; wave 110).
    TypeArraySize,
    /// The `frame`th entry (0-based) names no instruction start:
    /// `verify_error(bad_stackmap(frame, previous frame))` "StackMapTable
    /// error: bad offset".
    BadOffset { frame: usize },
}

/// [`StackMapRefusal`] of `raw` (the attribute's bytes after its length), in
/// HotSpot's reading order: per frame its type byte, then (for a type of 128
/// and above) the `offset_delta`, the reserved-type check, the verification
/// types (each checked as read), then the frame's offset against the
/// instruction starts; after the last frame, the attribute's size.
fn stack_map_reader_refusal(
    raw: &[u8],
    insns: &[cratonvm_reader::verified_code::VerifiedInstruction],
    cp: &ConstantPool,
    // The entry frame's locals, one slot width (1 or 2) per argument.
    initial_locals: &[u8],
    max_locals: u16,
    max_stack: u16,
) -> Option<StackMapRefusal> {
    use StackMapRefusal::Format;
    const END: StackMapRefusal = Format("access beyond the end of attribute");
    fn u1(raw: &[u8], pos: &mut usize) -> Result<u8, StackMapRefusal> {
        let b = *raw.get(*pos).ok_or(END)?;
        *pos += 1;
        Ok(b)
    }
    fn u2(raw: &[u8], pos: &mut usize) -> Result<u16, StackMapRefusal> {
        let hi = u1(raw, pos)?;
        let lo = u1(raw, pos)?;
        Ok(u16::from_be_bytes([hi, lo]))
    }
    let at = |offset: u32| {
        insns
            .binary_search_by_key(&offset, |v| v.pc)
            .ok()
            .map(|i| &insns[i].instruction)
    };
    // Each verification type answers its slot width (a `long` / `double`
    // takes two locals or stack slots).
    let vt = |pos: &mut usize| -> Result<u8, StackMapRefusal> {
        match u1(raw, pos)? {
            3 | 4 => Ok(2),
            0..=6 => Ok(1),
            7 => {
                let index = u2(raw, pos)?;
                let is_class = index != 0
                    && matches!(
                        cp.get(index),
                        Some(cratonvm_reader::constant_pool::ConstantPoolEntry::ClassReference { .. })
                    );
                if is_class {
                    Ok(1)
                } else {
                    Err(Format("bad class index"))
                }
            }
            8 => {
                let offset = u2(raw, pos)?;
                if matches!(at(u32::from(offset)), Some(Instruction::New(_))) {
                    Ok(1)
                } else {
                    Err(StackMapRefusal::UninitOffset)
                }
            }
            _ => Err(Format("bad verification type")),
        }
    };
    // `check_verification_type_array_size` (`class_format_error` "bad type
    // array size"; interpreter round i5 wave 110, lane L7): a frame's locals
    // in slots against `max_locals`, a stack against `max_stack`; a chop
    // past the previous frame's locals goes below zero.
    let size_ok = |size: i64, max: u16| size >= 0 && size <= i64::from(max);
    let scan = || -> Result<(), StackMapRefusal> {
        let mut pos = 0usize;
        let count = u2(raw, &mut pos)?;
        let mut previous: Option<u32> = None;
        // The previous frame's locals, one width per entry.
        let mut locals: Vec<u8> = initial_locals.to_vec();
        // JDK 25's `StackMapTable` constructor (interpreter round i5 wave 111,
        // lane L7, measured on w110): frames are read while the ATTRIBUTE has
        // bytes (`!reader->at_end()`), not `number_of_entries` times. Each
        // `next()` counts the frame and refuses one past the count
        // (`check_size`, "wrong attribute size"); when the bytes run out,
        // `check_end` refuses a count that was not reached ("wrong attribute
        // size"), so a count of 1 with no frame bytes is that text, while a
        // frame cut short is "access beyond the end of attribute". A count of
        // 0 reads nothing at all (`if (_frame_count > 0)`).
        if count == 0 {
            return Ok(());
        }
        let mut frame = 0usize;
        while pos < raw.len() {
            if frame >= usize::from(count) {
                return Err(Format("wrong attribute size"));
            }
            let frame_type = u1(raw, &mut pos)?;
            let delta = match frame_type {
                0..=63 => u32::from(frame_type),
                64..=127 => {
                    let width = vt(&mut pos)?;
                    if !size_ok(i64::from(width), max_stack) {
                        return Err(StackMapRefusal::TypeArraySize);
                    }
                    u32::from(frame_type - 64)
                }
                _ => {
                    let delta = u2(raw, &mut pos)?;
                    match frame_type {
                        128..=246 => return Err(Format("reserved frame type")),
                        247 => {
                            let width = vt(&mut pos)?;
                            if !size_ok(i64::from(width), max_stack) {
                                return Err(StackMapRefusal::TypeArraySize);
                            }
                        }
                        248..=250 => {
                            let chops = usize::from(251 - frame_type);
                            if chops > locals.len() {
                                return Err(StackMapRefusal::TypeArraySize);
                            }
                            locals.truncate(locals.len() - chops);
                        }
                        252..=254 => {
                            for _ in 0..(frame_type - 251) {
                                locals.push(vt(&mut pos)?);
                            }
                            let size: i64 = locals.iter().map(|&w| i64::from(w)).sum();
                            if !size_ok(size, max_locals) {
                                return Err(StackMapRefusal::TypeArraySize);
                            }
                        }
                        255 => {
                            locals.clear();
                            for _ in 0..u2(raw, &mut pos)? {
                                locals.push(vt(&mut pos)?);
                            }
                            let size: i64 = locals.iter().map(|&w| i64::from(w)).sum();
                            if !size_ok(size, max_locals) {
                                return Err(StackMapRefusal::TypeArraySize);
                            }
                            let mut stack: i64 = 0;
                            for _ in 0..u2(raw, &mut pos)? {
                                stack += i64::from(vt(&mut pos)?);
                            }
                            if !size_ok(stack, max_stack) {
                                return Err(StackMapRefusal::TypeArraySize);
                            }
                        }
                        // same_frame_extended
                        _ => {}
                    }
                    u32::from(delta)
                }
            };
            let offset = match previous {
                None => delta,
                Some(p) => p + delta + 1,
            };
            if at(offset).is_none() {
                return Err(StackMapRefusal::BadOffset { frame });
            }
            previous = Some(offset);
            frame += 1;
        }
        if frame != usize::from(count) {
            return Err(Format("wrong attribute size"));
        }
        Ok(())
    };
    scan().err()
}

/// The `LinkageError` HotSpot raises for `refusal` in `method`.
fn stack_map_refusal_error(
    refusal: StackMapRefusal,
    m: &HsMethod<'_>,
    class_name_arc: &Arc<str>,
    method: &ClassFileMethod,
    cp: &ConstantPool,
    max_locals: u16,
    max_stack: u16,
) -> LinkageError {
    let class_name: &str = class_name_arc;
    match refusal {
        StackMapRefusal::Format(text) => LinkageError::ClassFormatError {
            class_name: class_name.to_string(),
            message: format!("StackMapTable format error: {text}"),
        },
        StackMapRefusal::UninitOffset => LinkageError::ClassFormatError {
            class_name: class_name.to_string(),
            message: format!(
                "StackMapTable format error: bad offset for Uninitialized in method '{}'",
                crate::verifier::external_method_name(class_name, &method.name, &method.descriptor)
            ),
        },
        StackMapRefusal::TypeArraySize => LinkageError::ClassFormatError {
            class_name: class_name.to_string(),
            message: format!(
                "StackMapTable format error: bad type array size in method '{}'",
                crate::verifier::external_method_name(class_name, &method.name, &method.descriptor)
            ),
        },
        StackMapRefusal::BadOffset { frame } => {
            // Location is bci 0 (`bad_stackmap` carries no bci); the frame
            // printed is the one read before the bad entry (the method's
            // entry frame for the first).
            let (compact, _) = VerificationFrame::initial_frames(
                class_name_arc,
                &method.name,
                &method.descriptor,
                method.is_static(),
                max_locals,
                max_stack,
            );
            let previous = if frame == 0 {
                Some(HsFrame::declared(&compact, 0))
            } else {
                m.stack_map_raw
                    .and_then(|raw| StackMapTable::parse(raw).ok())
                    .and_then(|table| {
                        let offsets = table.absolute_offsets().ok()?;
                        let at = *offsets.get(frame - 1)?;
                        let declared =
                            build_declared_frames(&table, &compact, cp, class_name, &method.name)
                                .ok()?;
                        declared.get(&at).map(|f| HsFrame::declared(f, usize::from(at)))
                    })
            };
            LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: hs::render(
                    m,
                    "StackMapTable error: bad offset",
                    0,
                    &Reason::BadStackmap,
                    previous.as_ref(),
                    None,
                ),
            }
        }
    }
}

/// HotSpot's `StackMapReader::parse_verification_type` for an
/// `Uninitialized(offset)` entry: the offset must be the bci of a `new`
/// (`_code_data[offset] == NEW_OFFSET`), else `ClassFormatError`
/// "StackMapTable format error: bad offset for Uninitialized in method
/// '<external name>'" (`class_format_error`). The table is read frame by frame
/// before the walk; a frame whose own offset is not an instruction start is
/// HotSpot's "StackMapTable error: bad offset" (VerifyError), not replayed
/// here, so the scan stops there. Interpreter round i5 wave 108, lane L7.
fn uninitialized_offset_refusal(
    table: &StackMapTable,
    insns: &[cratonvm_reader::verified_code::VerifiedInstruction],
    class_name: &str,
    method: &ClassFileMethod,
) -> Option<LinkageError> {
    use cratonvm_reader::stack_map::{StackMapFrame, VerificationTypeInfo};
    let offsets = table.absolute_offsets().ok()?;
    // `insns` is in pc order (the canonical decode).
    let at = |offset: u16| {
        insns
            .binary_search_by_key(&u32::from(offset), |v| v.pc)
            .ok()
            .map(|i| &insns[i].instruction)
    };
    let new_at = |offset: u16| matches!(at(offset), Some(Instruction::New(_)));
    let starts_at = |offset: u16| at(offset).is_some();
    for (entry, &offset) in table.entries.iter().zip(offsets.iter()) {
        let types: Vec<&VerificationTypeInfo> = match entry {
            StackMapFrame::SameLocals1StackItem { stack, .. }
            | StackMapFrame::SameLocals1StackItemExtended { stack, .. } => vec![stack],
            StackMapFrame::AppendFrame { locals, .. } => locals.iter().collect(),
            StackMapFrame::FullFrame { locals, stack, .. } => {
                locals.iter().chain(stack.iter()).collect()
            }
            _ => Vec::new(),
        };
        for t in types {
            if let VerificationTypeInfo::Uninitialized { offset: new_offset } = t {
                if !new_at(*new_offset) {
                    return Some(LinkageError::ClassFormatError {
                        class_name: class_name.to_string(),
                        message: format!(
                            "StackMapTable format error: bad offset for Uninitialized in method '{}'",
                            crate::verifier::external_method_name(
                                class_name,
                                &method.name,
                                &method.descriptor
                            )
                        ),
                    });
                }
            }
        }
        if !starts_at(offset) {
            return None;
        }
    }
    None
}

/// HotSpot's `verify_exception_handler_targets` at `pc` (interpreter round i5
/// wave 101, lane L7): for each handler whose range covers `pc`, in table
/// order, the frame `current`'s locals with the handler's catch type as the
/// only stack slot must be assignable to the frame declared at the handler.
/// `Err` is HotSpot's whole message: "Expecting a stackmap frame at branch
/// target H" when no frame is declared there, else "Stack map does not match
/// the one at exception handler H". Nothing is allocated unless a check
/// fails.
///
/// `this_uninit` is HotSpot's flag of the exception frame
/// (`current_frame->flags() | this_uninit`; interpreter round i5 wave 108,
/// lane L7): `Some(flag)` compares it with the handler frame's after the
/// types (`FLAGS_MISMATCH`, "Current frame's flags are not assignable to
/// stack map frame's.") and prints it; `None` compares no flags
/// (`CRATONVM_VERIFY_THIS_UNINIT_FLAGS=0`).
#[cfg_attr(not(test), allow(dead_code))]
fn check_handler_frames_at(
    m: &HsMethod<'_>,
    pc: usize,
    current: &VerificationFrame,
    this_uninit: Option<bool>,
    handlers: &[(u16, u16, u16, VType)],
    declared_frames: &FxHashMap<u16, VerificationFrame>,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), String> {
    check_handler_frames_at_memo(
        m,
        pc,
        current,
        this_uninit,
        handlers,
        declared_frames,
        hierarchy,
        &[],
    )
}

/// [`check_handler_frames_at_memo`]'s per-handler verdict cells.
const CATCH_MEMO_UNKNOWN: u8 = 0;
const CATCH_MEMO_ASSIGNABLE: u8 = 1;
const CATCH_MEMO_REFUSED: u8 = 2;

/// [`check_handler_frames_at`] with `catch_memo[i]` caching handler `i`'s
/// "catch type is assignable to the declared frame's stack slot" verdict for
/// the method being walked (empty: no memo).
///
/// JIT round 23 wave 2 (proxy-define page): both operands are fixed per
/// handler (the catch type, and the stack slot of the frame declared at the
/// handler pc), so the verdict is the same at every pc of the range; asking
/// it per instruction cost a loader-aware name lookup pair per pc
/// (`RuntimeException` / `Error` -> `Throwable` over every proxy method). The
/// first ask is made at the same point as before, so the verifier loads it
/// records (first-seen order) are unchanged; a repeat ask would only
/// re-record names already recorded. The class store does not change during
/// one method's walk (the verifier loads nothing).
#[allow(clippy::too_many_arguments)]
fn check_handler_frames_at_memo(
    m: &HsMethod<'_>,
    pc: usize,
    current: &VerificationFrame,
    this_uninit: Option<bool>,
    handlers: &[(u16, u16, u16, VType)],
    declared_frames: &FxHashMap<u16, VerificationFrame>,
    hierarchy: &dyn ClassHierarchy,
    catch_memo: &[std::cell::Cell<u8>],
) -> Result<(), String> {
    for (index, (start, end, handler, catch_type)) in handlers.iter().enumerate() {
        if pc < usize::from(*start) || pc >= usize::from(*end) {
            continue;
        }
        let Some(declared) = declared_frames.get(handler) else {
            return Err(hs::render(
                m,
                &format!("Expecting a stackmap frame at branch target {handler}"),
                pc,
                &Reason::MissingStackmap,
                None,
                None,
            ));
        };
        // The exception frame: `current`'s locals, `[catch_type]`.
        let cur_locals: &[VType] = &current.locals;
        let tgt_locals: &[VType] = &declared.locals;
        let mut mismatch = None;
        if declared.stack.len() != 1 {
            mismatch = Some(hs::FrameMismatch::StackSize);
        } else {
            for (i, t) in tgt_locals.iter().enumerate() {
                let c = cur_locals.get(i).unwrap_or(&VType::Top);
                if !c.is_assignable_to(t, hierarchy) {
                    mismatch = Some(hs::FrameMismatch::Local(i));
                    break;
                }
            }
            if mismatch.is_none() {
                let cell = catch_memo.get(index);
                let assignable = match cell.map(std::cell::Cell::get) {
                    Some(CATCH_MEMO_ASSIGNABLE) => true,
                    Some(CATCH_MEMO_REFUSED) => false,
                    _ => {
                        let ok = catch_type.is_assignable_to(&declared.stack[0], hierarchy);
                        if let Some(cell) = cell {
                            cell.set(if ok { CATCH_MEMO_ASSIGNABLE } else { CATCH_MEMO_REFUSED });
                        }
                        ok
                    }
                };
                if !assignable {
                    mismatch = Some(hs::FrameMismatch::Stack(0));
                }
            }
        }
        let flags_mismatch = mismatch.is_none()
            && this_uninit == Some(true)
            && !locals_hold_uninit_this(declared);
        if mismatch.is_some() || flags_mismatch {
            let mut exc_frame = current.clone();
            exc_frame.clear_stack();
            // One slot: a fresh frame's `max_stack` is at least 1 whenever
            // a handler is reachable; an overflow here only loses the slot
            // from the printed frame.
            let _ = exc_frame.push(catch_type.clone());
            let _ = exc_frame.take_fault();
            let mut printed = HsFrame::current(&exc_frame, pc);
            if let Some(flag) = this_uninit {
                printed.this_uninit = flag;
            }
            let header = format!("Stack map does not match the one at exception handler {handler}");
            return Err(match mismatch {
                Some(mismatch) => hs::render_mismatch(
                    m,
                    &header,
                    pc,
                    usize::from(*handler),
                    mismatch,
                    &printed,
                    &exc_frame,
                    declared,
                ),
                None => hs::render_flags_mismatch(
                    m,
                    &header,
                    usize::from(*handler),
                    &printed,
                    declared,
                ),
            });
        }
    }
    Ok(())
}

/// What [`verify_method`]'s second walk records: the frame the instruction at
/// `pc` started from (interpreter round i5 wave 101, lane L7).
struct FrameCapture {
    pc: usize,
    frame: Option<VerificationFrame>,
}

/// The frame the instruction at `pc` started from, recovered by walking the
/// method again up to `pc` (HotSpot prints that frame -- its operands restored
/// -- in a VerifyError's `Current Frame:`; the first walk has already popped
/// them). Runs only when a method fails verification. `None` when the second
/// walk takes another road (the inference fallback) and never reaches `pc`.
#[cold]
#[allow(clippy::too_many_arguments)]
fn pre_instruction_frame(
    class_name_arc: &Arc<str>,
    method: &ClassFileMethod,
    cp: &ConstantPool,
    version: &ClassFileVersion,
    hierarchy: &dyn ClassHierarchy,
    strict_verification: bool,
    scratch: &mut VerifyScratch,
    pc: usize,
) -> Option<VerificationFrame> {
    let mut capture = FrameCapture { pc, frame: None };
    let _ = verify_method(
        class_name_arc,
        method,
        cp,
        version,
        hierarchy,
        strict_verification,
        scratch,
        None,
        Some(&mut capture),
    );
    capture.frame
}

/// Where HotSpot's split verifier stops on a method that has jumps or
/// handlers but no `StackMapTable` at all, if every instruction before it
/// verifies: `(bci, target)` of the first instruction (in code order) that
/// is a jump or lies in a handler's range. A store into a local checks its
/// handlers before the instruction, any other instruction after its own jump
/// targets (`ClassVerifier::verify_method`); a switch's default target is
/// checked first.
fn first_frameless_target(
    insns: &[cratonvm_reader::verified_code::VerifiedInstruction],
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> Option<(usize, usize)> {
    for v in insns {
        let bci = v.pc as usize;
        let handler = exception_table
            .iter()
            .find(|e| bci >= usize::from(e.start_pc) && bci < usize::from(e.end_pc))
            .map(|e| usize::from(e.handler_pc));
        let is_store = matches!(
            v.instruction,
            Instruction::Istore(_)
                | Instruction::Lstore(_)
                | Instruction::Fstore(_)
                | Instruction::Dstore(_)
                | Instruction::Astore(_)
        );
        if is_store {
            if let Some(h) = handler {
                return Some((bci, h));
            }
        }
        if let Ok(targets) = crate::verifier::instruction_branch_targets(&v.instruction, bci) {
            if let Some(&t) = targets.first() {
                if let Ok(t) = usize::try_from(t) {
                    return Some((bci, t));
                }
            }
        }
        if let Some(h) = handler {
            return Some((bci, h));
        }
    }
    None
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
        return Ok(VType::ObjectRef(super::vtype::with_well_known(|w| {
            Arc::clone(&w.throwable)
        })));
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

/// The first handler, in table order, whose typed catch type the hierarchy
/// already knows is not a `java/lang/Throwable`
/// ([`ClassHierarchy::catch_type_is_throwable`]), with the catch type's name.
/// A catch type that does not resolve to a class name is `catch_type_of`'s
/// refusal, later; it is skipped here.
fn first_non_throwable_catch_type<'e>(
    table: &'e [cratonvm_reader::attribute::ExceptionTableEntry],
    cp: &ConstantPool,
    class_name: &str,
    method: &ClassFileMethod,
    hierarchy: &dyn ClassHierarchy,
) -> Option<(&'e cratonvm_reader::attribute::ExceptionTableEntry, Arc<str>)> {
    for entry in table {
        if entry.catch_type == 0 {
            continue;
        }
        let Ok(VType::ObjectRef(name)) = catch_type_of(entry, cp, class_name, &method.name) else {
            continue;
        };
        if hierarchy.catch_type_is_throwable(&name) == Some(false) {
            return Some((entry, name));
        }
    }
    None
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
/// The raw `StackMapTable` attribute among a `Code` attribute's attributes
/// (public for the VM's catch-type refusal text; interpreter round i5 wave
/// 102, lane L7).
pub fn find_stack_map_table(attributes: &[Attribute]) -> Option<&[u8]> {
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
    let mut frames = FxHashMap::with_capacity_and_hasher(table.entries.len(), Default::default());
    // Round 7 audit fix (MED #8): u32-internal accumulation with
    // `absolute > u16::MAX` rejection surfaces here as a verify error.
    let offsets = table
        .absolute_offsets()
        .map_err(|e| LinkageError::VerifyError {
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            message: format!("StackMapTable absolute_offsets: {e}"),
        })?;

    // Each frame is derived from the one before it, so build them in order in a
    // vector (the previous frame is then just a borrow, not a clone) and move
    // them into the map afterwards; a repeated offset still ends up holding the
    // later frame, as the insert-as-you-go form did.
    let mut built: Vec<(u16, VerificationFrame)> = Vec::with_capacity(table.entries.len());

    for (i, entry) in table.entries.iter().enumerate() {
        let offset = offsets[i];
        let prev_frame = match built.last() {
            Some((_, frame)) => frame,
            None => initial_frame,
        };

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

        built.push((offset, new_frame));
    }

    for (offset, frame) in built {
        frames.insert(offset, frame);
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
    scratch: &mut VerifyScratch,
    // Report libverify's texts (major below 50; interpreter round i5 wave
    // 102, lane L7).
    old_texts: bool,
) -> Result<MethodTypeMaps, LinkageError> {
    // libverify's whole message for `reason`.
    let old_error = |reason: &str| LinkageError::VerifyError {
        class_name: class_name.to_string(),
        method_name: method.name.to_string(),
        message: old::message(class_name, &method.name, &method.descriptor, reason),
    };
    // libverify refuses a stack merge with no common type at the merge
    // ("Mismatched stack types"); this worklist merged it to `Top` and
    // refused only a later use (`CRATONVM_VERIFY_CONTROL_RULES`, default on).
    let old_merges = old_texts && crate::loader_flags().verify_control_rules;
    // libverify's receiver rule over merged types (interpreter round i5 wave
    // 110, lane L7; `VerifyScratch::old_receiver_merged`).
    scratch.apply_old_receiver_rule(&method.name, &method.descriptor, true);
    // Every merge of this walk goes through `watch`: libverify's array merge,
    // and a NAMED `Arc` for each merge that fell back to `java/lang/Object`
    // over a class not loaded (waves 111-112), which the receiver rule
    // accepts; every other merged type is libverify-exact.
    let watch = MergeFallbackWatch {
        inner: hierarchy,
        fallbacks: std::cell::RefCell::new(Vec::new()),
    };
    let hierarchy: &dyn ClassHierarchy = &watch;
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

    // libverify (`run_dataflow`) visits the changed instructions in code
    // order, sweep after sweep; with its texts on, so does this walk, so the
    // first refusal is the one HotSpot reports (a LIFO worklist reaches a
    // bad merge's later use before the merge's second edge).
    let mut sweep_from = 0usize;
    loop {
        let pc = if old_texts {
            let next = enqueued
                .iter()
                .copied()
                .filter(|&p| p >= sweep_from)
                .min()
                .or_else(|| enqueued.iter().copied().min());
            let Some(pc) = next else {
                break;
            };
            worklist.retain(|&w| w != pc);
            sweep_from = pc + 1;
            pc
        } else {
            match worklist.pop() {
                Some(pc) => pc,
                None => break,
            }
        };
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
        let result = match verify_instruction_with(
            &insn,
            pc,
            &mut current,
            cp,
            class_name,
            &method.name,
            &method.descriptor,
            hierarchy,
            scratch,
        ) {
            Ok(result) => result,
            Err(e) => {
                // libverify's reason, replayed from the frame this
                // instruction started from (still `frame_at[pc]`).
                let fault = current.take_fault();
                let reason = if old_texts {
                    frame_at.get(&pc).and_then(|pre| {
                        old::instruction_reason(&old::OldCtx {
                            insn: &insn,
                            pre,
                            fault: fault.as_ref(),
                            cp,
                            class_name,
                            method_name: &method.name,
                            descriptor: &method.descriptor,
                            is_static: method.is_static(),
                        })
                    })
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return Err(old_error(&reason));
                }
                return Err(match e {
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
                        message: format!("at bytecode offset {pc}: {message}"),                    },
                    other => other,                });            }        };

        // Control may not fall through the end of the code
        // (`CRATONVM_VERIFY_CONTROL_RULES`, default on).
        if result.falls_through
            && next_pc >= bytecode.len()
            && crate::loader_flags().verify_control_rules
        {
            if old_texts {
                return Err(old_error("Falling off the end of the code"));
            }
            return Err(LinkageError::VerifyError {
                class_name: class_name.to_string(),
                method_name: method.name.to_string(),
                message: hs::render(
                    &HsMethod {
                        class_name,
                        method_name: &method.name,
                        descriptor: &method.descriptor,
                        code: bytecode,
                        exception_table: &code_attr.exception_table,
                        stack_map_raw: find_stack_map_table(&code_attr.attributes),
                    },
                    "Control flow falls through code end",
                    bytecode.len(),
                    &Reason::BadCode,
                    None,
                    None,
                ),
            });
        }

        // Propagate to fall-through successor
        if result.falls_through && next_pc < bytecode.len() {
            if old_merges {
                if let Some(reason) = frame_at
                    .get(&next_pc)
                    .and_then(|existing| old::merge_reason(existing, &current, hierarchy))
                {
                    return Err(old_error(&reason));
                }
            }
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
            if old_merges {
                if let Some(reason) = frame_at
                    .get(&target_pc)
                    .and_then(|existing| old::merge_reason(existing, &current, hierarchy))
                {
                    return Err(old_error(&reason));
                }
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
                if old_merges {
                    if let Some(reason) = frame_at
                        .get(&handler_pc)
                        .and_then(|existing| old::merge_reason(existing, &handler_frame, hierarchy))
                    {
                        return Err(old_error(&reason));
                    }
                }
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
mod tests;
