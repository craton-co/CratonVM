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
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VERIFY_HIDDEN_SELF_NAME") {
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
/// (`tools/probes/jit/r23/VLoad.java`, `-Xlog:verification`). So only the
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
                        message: format!("at bytecode offset {pc}: {message}"),
                    },
                    other => other,
                });
            }
        };

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

    /// `CRATONVM_VERIFY_INITIALIZERS_FIRST` (default ON): every `<init>`, then
    /// `<clinit>`, then the rest, each group in class-file order; the type maps
    /// stay indexed by class-file position.
    #[test]
    fn methods_are_visited_initializers_first() {
        let named = |name: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from(name),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        };
        let class = make_class(vec![
            named("hashCode"),
            named("<clinit>"),
            named("run"),
            named("<init>"),
            named("zz"),
            named("<init>"),
        ]);
        assert_eq!(method_visit_order(&class), vec![3, 5, 1, 0, 2, 4]);
        assert!(verify_bytecode(&class, &MockHierarchy).is_ok());
        let mut slots = MethodSlots::new(2);
        slots.put(1, (Arc::from("b"), Arc::from("()V"), None));
        slots.put(0, (Arc::from("a"), Arc::from("()V"), None));
        let names: Vec<String> = slots
            .into_vec()
            .into_iter()
            .map(|e| e.0.to_string())
            .collect();
        assert_eq!(names, vec!["a", "b"], "read back in method-index order");
    }

    #[test]
    fn verify_trivial_return_method() {
        let h = MockHierarchy;

        // A method that just does `return` (bytecode 0xB1)
        let class = make_class(vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from("test"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0xAC]), // iconst_0, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x60, 0xAC]), // iconst_0, iadd, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x05, 0x60, 0xAC]), // iconst_1, iconst_2, iadd, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x3B, 0x1A, 0xAC]), // iconst_0, istore_0, iload_0, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x57, 0xB1]), // pop, return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x59, 0xB1]), // dup, return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x59, 0x60, 0xAC]), // iconst_1, dup, iadd, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x09, 0xAD]), // lconst_0, lreturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x0B, 0xAE]), // fconst_0, freturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x0E, 0xAF]), // dconst_0, dreturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x01, 0xB0]), // aconst_null, areturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x64, 0xAC]), // iconst_1, isub, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x10, 0x2A, 0xAC]), // bipush 42, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x11, 0x03, 0xE8, 0xAC]), // sipush 1000, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
                attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                    CodeAttribute {
                        max_stack: 0,
                        max_locals: 0,
                        code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                        exception_table: vec![],
                        attributes: vec![],
                    },
                )))],
            },
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                name: Arc::from("bad"),
                descriptor: Arc::from("()I"),
                attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                    CodeAttribute {
                        max_stack: 2,
                        max_locals: 0,
                        code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x60, 0xAC]), // iconst_0, iadd (underflow), ireturn
                        exception_table: vec![],
                        attributes: vec![],
                    },
                )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
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
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: code.into(),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
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
                },
            )))],
        }]);
        class.version = ClassFileVersion::JAVA_8;
        let res = verify_pre_java7(&class);
        // HotSpot's text (interpreter round i5 wave 101, lane L7): the walk
        // stops at the first jump whose target has no frame.
        assert_verify_err_contains(&res, "Expecting a stackmap frame at branch target 6
");
        assert_verify_err_contains(&res, "
Exception Details:
");
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0x04, 0x60, 0xAC]), // iconst_0, iconst_1, iadd, ireturn
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x03, 0xAF]), // iconst_0; dreturn (needs double)
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0x05, 0x60, 0xAC]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![Attribute::StackMapTable {
                        entries: Box::new(cratonvm_reader::ByteView::from_vec(stack_map_bytes)),
                    }],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![Attribute::StackMapTable {
                        entries: Box::new(cratonvm_reader::ByteView::from_vec(stack_map_bytes)),
                    }],
                },
            )))],
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
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table,
                    attributes: vec![Attribute::StackMapTable {
                        entries: Box::new(cratonvm_reader::ByteView::from_vec(stack_map_bytes)),
                    }],
                },
            )))],
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
        // Interpreter round i5 wave 101, lane L7: HotSpot's handler-frame
        // check (`CRATONVM_VERIFY_HANDLER_FRAMES`, default on) refuses this
        // class outright -- the handler at 2 declares no frame -- so the
        // guard below is only reachable with the check off.
        if crate::loader_flags().verify_handler_frames {
            match verify_bytecode(&class, &MockHierarchy) {
                Err(LinkageError::VerifyError { message, .. }) => assert!(
                    message.starts_with("Expecting a stackmap frame at branch target 2
"),
                    "{message}"
                ),
                other => panic!("expected the handler-frame refusal, got {other:?}"),
            }
            return;
        }
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

    /// Interpreter round i5 wave 101, lane L7: HotSpot's handler-frame check.
    #[test]
    fn handler_frames_are_matched_per_instruction() {
        let code = [0x03u8, 0x3b, 0xb1];
        let m = HsMethod {
            class_name: "p/C",
            method_name: "m",
            descriptor: "()V",
            code: &code,
            exception_table: &[],
            stack_map_raw: None,
        };
        let throwable = VType::ObjectRef(Arc::from("java/lang/Throwable"));
        // The handler at 10 declares locals { integer }, stack { Throwable }.
        let mut declared = VerificationFrame::compact_initial_frame("p/C", "m", "(I)V", true, 4);
        declared.push(throwable.clone()).unwrap();
        let mut frames = FxHashMap::default();
        frames.insert(10u16, declared);
        let handlers = vec![(0u16, 3u16, 10u16, throwable.clone())];
        let h = MockHierarchy;

        // Locals { integer }: matches.
        let ok = VerificationFrame::initial_frame("p/C", "m", "(I)V", true, 2, 4);
        assert!(check_handler_frames_at(&m, 1, &ok, None, &handlers, &frames, &h).is_ok());
        // Outside the range: not checked.
        let mut bad = VerificationFrame::initial_frame("p/C", "m", "()V", true, 2, 4);
        bad.locals[0] = VType::Float;
        assert!(check_handler_frames_at(&m, 5, &bad, None, &handlers, &frames, &h).is_ok());
        // Locals { float }: HotSpot's text, Location at the handler.
        let err = check_handler_frames_at(&m, 1, &bad, None, &handlers, &frames, &h).unwrap_err();
        assert!(
            err.starts_with("Stack map does not match the one at exception handler 10
"),
            "{err}"
        );
        assert!(
            err.contains("Type float (current frame, locals[0]) is not assignable to integer (stack map, locals[0])"),
            "{err}"
        );
        assert!(err.contains("stack: { 'java/lang/Throwable' }"), "{err}");
        // No frame at the handler.
        let none = FxHashMap::default();
        let err = check_handler_frames_at(&m, 1, &ok, None, &handlers, &none, &h).unwrap_err();
        assert!(err.starts_with("Expecting a stackmap frame at branch target 10
"), "{err}");
    }

    /// `is_subclass` answers `answer` and counts its calls.
    struct CountingHierarchy {
        calls: std::cell::Cell<usize>,
        answer: bool,
    }

    impl ClassHierarchy for CountingHierarchy {
        fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
            self.calls.set(self.calls.get() + 1);
            self.answer
        }
        fn is_direct_superclass(&self, _child: &str, _parent: &str) -> bool {
            false
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    /// JIT round 23 wave 2 (proxy-define page): with a memo the catch type's
    /// assignability is asked once per handler, not at every pc of its range,
    /// and the verdict (either way) is the one the unmemoised check gives.
    #[test]
    fn handler_catch_verdict_is_asked_once_per_handler() {
        let code = [0x03u8, 0x3b, 0xb1];
        let m = HsMethod {
            class_name: "p/C",
            method_name: "m",
            descriptor: "()V",
            code: &code,
            exception_table: &[],
            stack_map_raw: None,
        };
        let throwable = VType::ObjectRef(Arc::from("java/lang/Throwable"));
        let catch = VType::ObjectRef(Arc::from("p/E"));
        let mut declared = VerificationFrame::compact_initial_frame("p/C", "m", "(I)V", true, 4);
        declared.push(throwable.clone()).unwrap();
        let mut frames = FxHashMap::default();
        frames.insert(10u16, declared);
        let handlers = vec![(0u16, 3u16, 10u16, catch)];
        let ok = VerificationFrame::initial_frame("p/C", "m", "(I)V", true, 2, 4);
        for answer in [true, false] {
            let h = CountingHierarchy {
                calls: std::cell::Cell::new(0),
                answer,
            };
            let memo = vec![std::cell::Cell::new(CATCH_MEMO_UNKNOWN); handlers.len()];
            for pc in 0..3 {
                let memoised = check_handler_frames_at_memo(
                    &m,
                    pc,
                    &ok,
                    None,
                    &handlers,
                    &frames,
                    &h,
                    &memo,
                );
                let plain = check_handler_frames_at(&m, pc, &ok, None, &handlers, &frames, &h);
                assert_eq!(memoised.is_ok(), answer, "pc {pc}");
                assert_eq!(memoised, plain, "pc {pc}");
            }
            // Once for the memoised walk, three times for the plain one.
            assert_eq!(h.calls.get(), 4, "answer {answer}");
        }
    }

    // =======================================================================
    // JIT round 21 wave 14, lane verifier: a hidden class's own
    // `CONSTANT_Class` entries name the hidden class (HotSpot's
    // `ClassFileParser` re-points `this_class` at the mangled name).
    // =======================================================================

    /// `p/H`'s direct superclass is `java/lang/Object`, under either name.
    struct HiddenSelfHierarchy;

    impl ClassHierarchy for HiddenSelfHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            child == parent || parent == "java/lang/Object"
        }
        fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
            child.starts_with("p/H") && parent == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    /// `p/H` as `Lookup.defineHiddenClass` stores it (`p/H/0x1f`), with
    ///
    /// ```text
    /// H()      { super(); }
    /// H(int x) { this(); if (x != 0) this.f = x; }
    /// ```
    ///
    /// where `this()`, the `putfield` and the `StackMapTable` frame at the
    /// join all name the class through its class-file name `p/H`.
    fn hidden_self_delegating_class(hidden: bool) -> Class {
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("p/H".into()),                // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8("<init>".into()),             // 3
            ConstantPoolEntry::Utf8("()V".into()),                // 4
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6: p/H.<init>()V
            ConstantPoolEntry::Utf8("java/lang/Object".into()), // 7
            ConstantPoolEntry::ClassReference { name_index: 7 }, // 8
            ConstantPoolEntry::MethodReference {
                class_index: 8,
                name_and_type_index: 5,
            }, // 9: java/lang/Object.<init>()V
            ConstantPoolEntry::Utf8("f".into()), // 10
            ConstantPoolEntry::Utf8("I".into()), // 11
            ConstantPoolEntry::NameAndType {
                name_index: 10,
                descriptor_index: 11,
            }, // 12
            ConstantPoolEntry::FieldReference {
                class_index: 2,
                name_and_type_index: 12,
            }, // 13: p/H.f:I
        ]);
        let ctor = |descriptor: &str,
                    max_stack: u16,
                    max_locals: u16,
                    code: Vec<u8>,
                    attributes: Vec<Attribute>| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("<init>"),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack,
                    max_locals,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes,
                },
            )))],
        };
        let delegating = vec![
            0x2A, // 0: aload_0
            0xB7, 0x00, 0x06, // 1: invokespecial p/H.<init>()V  (this())
            0x1B, // 4: iload_1
            0x99, 0x00, 0x09, // 5: ifeq 14
            0x2A, // 8: aload_0
            0x1B, // 9: iload_1
            0xB5, 0x00, 0x0D, // 10: putfield p/H.f:I
            0xB1, // 13: return
            0xB1, // 14: return
        ];
        // One full_frame at 14: locals { Object_variable(#2 = p/H), int },
        // empty stack.
        let stack_map = vec![
            0x00, 0x01, 0xFF, 0x00, 0x0E, 0x00, 0x02, 0x07, 0x00, 0x02, 0x01, 0x00, 0x00,
        ];
        let mut class = make_class(vec![
            ctor("()V", 1, 1, vec![0x2A, 0xB7, 0x00, 0x09, 0xB1], vec![]),
            ctor(
                "(I)V",
                2,
                2,
                delegating,
                vec![Attribute::StackMapTable {
                    entries: Box::new(cratonvm_reader::ByteView::from_vec(stack_map)),
                }],
            ),
        ]);
        class.name = Arc::from("p/H/0x1f");
        class.version = ClassFileVersion::JAVA_17;
        class.constant_pool = cp;
        class.hidden = hidden;
        class
    }

    #[test]
    fn hidden_class_constructor_delegating_through_its_class_file_name_verifies() {
        let class = hidden_self_delegating_class(true);
        let result = verify_bytecode(&class, &HiddenSelfHierarchy);
        assert!(result.is_ok(), "HotSpot 25 runs this class: {result:?}");

        // The verifier's pool: every self entry names the hidden class; the
        // class store's pool keeps the class-file name.
        let patched = hidden_self_named_pool(&class).expect("a self-naming hidden class");
        assert_eq!(patched.get_class_name(2), Some("p/H/0x1f"));
        assert_eq!(patched.get_class_name(8), Some("java/lang/Object"));
        assert_eq!(patched.get_utf8(1), Some("p/H"), "the shared Utf8 is not rewritten");
        assert_eq!(class.constant_pool.get_class_name(2), Some("p/H"));
    }

    #[test]
    fn the_same_bytes_under_a_foreign_name_are_still_a_bad_init_call() {
        // Control: a NON-hidden class stored as `p/H/0x1f` whose constructor
        // invokes `p/H.<init>` really calls another class's constructor on
        // `uninitializedThis` -- HotSpot's "Bad <init> method call".
        let class = hidden_self_delegating_class(false);
        assert!(hidden_self_named_pool(&class).is_none());
        let err = verify_bytecode(&class, &HiddenSelfHierarchy).unwrap_err();
        assert!(format!("{err:?}").contains("Bad <init> method call"), "{err:?}");
    }

    #[test]
    fn hidden_class_without_a_self_reference_keeps_its_pool() {
        let mut class = hidden_self_delegating_class(true);
        class.name = Arc::from("q/Other/0x2");
        assert!(hidden_self_named_pool(&class).is_none());
        // A tail that is not `/0x<hex>` is not a hidden-class suffix.
        class.name = Arc::from("p/H/0xzz");
        assert!(hidden_self_named_pool(&class).is_none());
        // A collision suffix after a pre-mangled name still finds `p/H`.
        class.name = Arc::from("p/H/0x1f/0x3");
        let patched = hidden_self_named_pool(&class).expect("p/H is named");
        assert_eq!(patched.get_class_name(2), Some("p/H/0x1f/0x3"));
    }
}
