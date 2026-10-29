// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Instruction type effects for bytecode verification.
//!
//! Each JVM instruction has a well-defined effect on the operand stack and local
//! variables. This module describes these effects in terms of `VType` — the
//! verifier applies them to a `VerificationFrame` as it walks bytecode.
//!
//! This is the core of Pass 3 verification (JVM spec 4.10.1).

use std::sync::Arc;

use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::instruction::Instruction;

use super::verify_frame::{HsType, VerificationFrame, VerifyFault};
use super::vtype::{return_type_from_descriptor, with_well_known, ClassHierarchy, VType};
use cratonvm_types::error::LinkageError;

/// The result of verifying a single instruction's type effects.
#[derive(Debug)]
pub struct InsnVerifyResult {
    /// Whether control falls through to the next instruction.
    pub falls_through: bool,
    /// Branch targets (absolute offsets) this instruction may jump to.
    pub branch_targets: BranchTargets,
}

/// The branch targets of one instruction: a slice that is stored inline when
/// there is at most one (every `if*`, `goto`, `jsr`, `ret`) and on the heap
/// only for a switch. SBR round 5, lane verify: this used to be a `Vec<u16>`,
/// i.e. one allocation per branch instruction of every verified method.
#[derive(Debug, Default)]
pub struct BranchTargets {
    inline: [u16; 1],
    inline_len: u8,
    heap: Vec<u16>,
}

impl BranchTargets {
    /// No target.
    #[inline]
    pub fn none() -> Self {
        Self::default()
    }

    /// Exactly one target.
    #[inline]
    pub fn one(target: u16) -> Self {
        Self {
            inline: [target],
            inline_len: 1,
            heap: Vec::new(),
        }
    }

    /// The targets, in order.
    #[inline]
    pub fn as_slice(&self) -> &[u16] {
        if self.heap.is_empty() {
            &self.inline[..self.inline_len as usize]
        } else {
            &self.heap
        }
    }
}

impl From<Vec<u16>> for BranchTargets {
    fn from(targets: Vec<u16>) -> Self {
        Self {
            inline: [0],
            inline_len: 0,
            heap: targets,
        }
    }
}

impl std::ops::Deref for BranchTargets {
    type Target = [u16];

    #[inline]
    fn deref(&self) -> &[u16] {
        self.as_slice()
    }
}

impl<'a> IntoIterator for &'a BranchTargets {
    type Item = &'a u16;
    type IntoIter = std::slice::Iter<'a, u16>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl PartialEq<Vec<u16>> for BranchTargets {
    fn eq(&self, other: &Vec<u16>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

/// Verify the type effects of a single instruction on the verification frame.
///
/// Modifies `frame` in place to reflect the instruction's stack/local effects.
/// Returns branch targets and whether control falls through.
///
/// `method_descriptor` is the descriptor of the method being verified (e.g.
/// `"(I)Ljava/lang/String;"`). It is required so that `areturn` can check the
/// returned reference is assignable to the method's declared return type
/// (JVMS §4.10.1.6 / §6.5 areturn) — without it the verifier would accept any
/// reference for `areturn`, a soundness hole.
pub fn verify_instruction(
    insn: &Instruction,
    pc: usize,
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    hierarchy: &dyn ClassHierarchy,
) -> Result<InsnVerifyResult, LinkageError> {
    verify_instruction_with(
        insn,
        pc,
        frame,
        cp,
        class_name,
        method_name,
        method_descriptor,
        hierarchy,
        &mut VerifyScratch::default(),
    )
}

/// What the verifier resolves from one constant pool, kept across the
/// instructions (and methods) of one class.
///
/// SBR round 5, lane verify: every field access, invoke, `anewarray` and so on
/// used to re-resolve its constant pool entry from scratch -- two `String`
/// copies for the name and the descriptor, a descriptor well-formedness walk,
/// then a second walk that allocated an `Arc<str>` per object-typed parameter.
/// A class that calls `String.length()` forty times paid that forty times. The
/// first resolution of an entry is done by exactly the code that always did it
/// (so its rejections, messages included, are unchanged -- a rejection is never
/// cached); the result of a SUCCESSFUL one is kept here, keyed by constant pool
/// index. One scratch must only ever be used with one constant pool.
#[derive(Default)]
pub struct VerifyScratch {
    fields: rustc_hash::FxHashMap<u16, VType>,
    methods: rustc_hash::FxHashMap<u16, std::rc::Rc<MethodRefInfo>>,
    array_types: rustc_hash::FxHashMap<u16, VType>,
    /// The class file is major <= 50: HotSpot verifies it with the old
    /// type-inferencing verifier (`check_code.c`; a version-50 class fails
    /// over to it), whose linkage-rule messages differ from the split
    /// verifier's. See [`VerifyScratch::for_class_version`].
    old_verifier: bool,
    /// The class file is major < 50: a refusal is worded as libverify's.
    /// A major-50 class refused by both verifiers reports the SPLIT
    /// verifier's message (`Verifier::verify`), so its texts follow the split
    /// verifier although its rules are the old one's where they are laxer.
    /// Interpreter round i5 wave 109, lane L7.
    old_texts: bool,
    /// The walk is a type-map collection that makes no load decision
    /// (`verifier::collect_class_type_maps`): the JVMS §4.10.1.8 / §4.10.1.9
    /// linkage rules are not applied, so they cannot cost a method its maps.
    skip_linkage_rules: bool,
    /// Set when [`check_protected_receiver`] or [`check_invokespecial_target`]
    /// refused an instruction; read (and cleared) by
    /// [`VerifyScratch::take_linkage_rule_violation`].
    linkage_rule_violation: bool,
    /// [`linkage_rule_protected_candidate`]'s answer per member reference
    /// (receiver-independent, so asked of the hierarchy once per entry).
    protected_rule: rustc_hash::FxHashMap<u16, bool>,
    /// HotSpot's receiver rule (`pop_stack(ref_class_type)` for
    /// `invokevirtual` / `getfield` / `putfield`, `pop_stack(current_type())`
    /// for a non-`<init>` `invokespecial`) applies: a split-verifier class
    /// (major >= 51) and `CRATONVM_VERIFY_RECEIVER_ASSIGNABLE` on (default).
    /// Interpreter round i5 wave 101, lane L7.
    receiver_rule: bool,
    /// [`check_receiver_assignable`]'s answers, target class -> receiver type
    /// -> assignable. One scratch serves one class (one defining loader), so
    /// this is the per-(loader, from, to) cache of that class's walk.
    receiver_assignable: rustc_hash::FxHashMap<Arc<str>, rustc_hash::FxHashMap<Arc<str>, bool>>,
    /// The `new` sites of the method being walked: pc -> the instruction's
    /// `CONSTANT_Class` index, for `verify_invoke_init`'s "Call to wrong
    /// <init> method" (interpreter round i5 wave 107, lane L7). Valid for the
    /// method whose name and descriptor pointers are `new_sites_method`;
    /// another method's instruction clears it (a missing entry only skips the
    /// check, it never refuses).
    new_sites: rustc_hash::FxHashMap<u16, u16>,
    new_sites_method: (usize, usize),
    /// The class file is major >= 50: `verify_invoke_init`'s protected rule
    /// applies ("Bad access to protected <init> method"; libverify checks it
    /// only above major 49). Interpreter round i5 wave 108, lane L7.
    init_protected_rule: bool,
    /// The class file is major < 49: libverify's `verify_opcode_operands`
    /// does not let `ldc` / `ldc_w` load a `CONSTANT_Class`
    /// (`LDC_CLASS_MAJOR_VERSION`), "Illegal type in constant pool".
    /// Interpreter round i5 wave 109, lane L7.
    ldc_class_refused: bool,
    /// The method (name and descriptor pointers) for which the receiver rule
    /// applies below major 51: a straight-line method (no branch, no
    /// handler), whose types come from no merge, so the inference walk's
    /// coarse merges over unloaded classes cannot cause a false refusal
    /// (interpreter round i5 wave 109, lane L7;
    /// `i109-L7-old-verifier-accepts-unassignable-receivers`). `(0, 0)` for
    /// none. libverify checks every receiver ("Incompatible object argument
    /// for function call").
    old_receiver_method: (usize, usize),
    /// [`Self::old_receiver_method`] names the method being walked (set per
    /// instruction by [`verify_instruction_with`]).
    old_receiver_rule_on: bool,
    /// [`Self::old_receiver_method`] is walked by the inference worklist,
    /// whose merges can be coarse (interpreter round i5 wave 110, lane L7):
    /// `VType::merge` gives the loaded hierarchy's exact common superclass,
    /// libverify's `merge_fullinfo_types` answer, but falls back to
    /// `java/lang/Object` when a class is not loaded. Since wave 112 the walk's
    /// hierarchy marks each such fallback VALUE
    /// (`ClassHierarchy::is_fallback_merge`), and only those receivers are
    /// accepted (a missed refusal, never a false one).
    old_receiver_merged: bool,
    /// Walk every method as if it had no StackMapTable: the libverify pass of
    /// a major-50 class's failover (`bytecode_verifier::
    /// verify_major_50_with_failover`; interpreter round i5 wave 112, lane
    /// L7). libverify never reads the table.
    ignore_stack_map: bool,
    /// JIT round 23 wave 5 (`CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE`): the
    /// previous method's emptied type-map row vectors, reused by the next
    /// method of this class's walk (`crate::type_maps::TypeMapSpare`).
    type_map_spare: crate::type_maps::TypeMapSpare,
    /// JIT round 23 wave 8f (`CRATONVM_LINK_TYPE_MAP_WALK_LOADS_ONLY`): the
    /// walk is the link's verifier-load walk for a class whose maps are
    /// already published, so `verifier::verify_method_typestate` records no
    /// type-map row (the rows would be discarded: first writer wins). The
    /// walk itself, and so every verifier load it records, is unchanged.
    skip_type_map_rows: bool,
    /// Phase 5 fast boot (lane p5-define): the store the walk publishes into
    /// strips every per-pc row at install (`TypeMapStore::drops_rows`, the
    /// default), so the walk's builders keep the verdicts only
    /// (`MethodTypeMapsBuilder::verdict_only`): the published maps are the
    /// same bytes, and the rows are never built to be thrown away.
    verdict_only_type_maps: bool,
}

impl VerifyScratch {
    /// See [`Self::skip_type_map_rows`].
    pub(crate) fn without_type_map_rows(mut self) -> Self {
        self.skip_type_map_rows = true;
        self
    }

    /// See [`Self::verdict_only_type_maps`]: build verdict-only maps when
    /// `store` strips rows at install.
    pub(crate) fn for_store(mut self, store: &crate::type_maps::TypeMapStore) -> Self {
        self.verdict_only_type_maps = store.drops_rows();
        self
    }

    /// See [`Self::verdict_only_type_maps`].
    pub(crate) fn verdict_only_type_maps(&self) -> bool {
        self.verdict_only_type_maps
    }

    /// Does this walk record type-map rows? (See [`Self::skip_type_map_rows`].)
    pub(crate) fn records_type_map_rows(&self) -> bool {
        !self.skip_type_map_rows
    }

    /// See [`Self::type_map_spare`].
    pub(crate) fn type_map_spare_mut(&mut self) -> &mut crate::type_maps::TypeMapSpare {
        &mut self.type_map_spare
    }
}

impl VerifyScratch {
    /// Apply the receiver rule to the method `(name, descriptor)` of a class
    /// below major 51 (see [`Self::old_receiver_method`]); `merged` for a
    /// method the inference worklist walks ([`Self::old_receiver_merged`]).
    /// A no-op above major 50 (the rule already applies) and with
    /// `CRATONVM_VERIFY_RECEIVER_ASSIGNABLE` off.
    pub(crate) fn apply_old_receiver_rule(&mut self, name: &str, descriptor: &str, merged: bool) {
        if self.old_verifier && crate::loader_flags().verify_receiver_assignable {
            self.old_receiver_method = (name.as_ptr() as usize, descriptor.as_ptr() as usize);
            self.old_receiver_merged = merged;
        }
    }

    /// See [`Self::ignore_stack_map`].
    pub(crate) fn ignoring_stack_maps(mut self) -> Self {
        self.ignore_stack_map = true;
        self
    }

    /// See [`Self::ignore_stack_map`].
    pub(crate) fn ignores_stack_map(&self) -> bool {
        self.ignore_stack_map
    }

    /// [`Self::new_sites`] for the method named `(name, descriptor)`: cleared
    /// when the previous instruction belonged to another method.
    fn new_sites_for(
        &mut self,
        name: &str,
        descriptor: &str,
    ) -> &mut rustc_hash::FxHashMap<u16, u16> {
        let key = (name.as_ptr() as usize, descriptor.as_ptr() as usize);
        if self.new_sites_method != key {
            self.new_sites_method = key;
            self.new_sites.clear();
        }
        &mut self.new_sites
    }
}

impl VerifyScratch {
    /// A scratch for a class of class-file major version `major`.
    pub fn for_class_version(major: u16) -> Self {
        VerifyScratch {
            old_verifier: major <= 50,
            old_texts: major < 50,
            receiver_rule: major > 50 && crate::loader_flags().verify_receiver_assignable,
            init_protected_rule: major >= 50,
            ldc_class_refused: major < 49,
            ..VerifyScratch::default()
        }
    }

    /// Do not apply the linkage rules (a map-only walk).
    pub(crate) fn without_linkage_rules(mut self) -> Self {
        self.skip_linkage_rules = true;
        // A map-only walk of an unverified (e.g. boot) class: an old `ldc`
        // of a class literal must not cost the method its maps.
        self.ldc_class_refused = false;
        self
    }

    /// Did a linkage rule refuse an instruction since the last call?
    pub(crate) fn take_linkage_rule_violation(&mut self) -> bool {
        std::mem::take(&mut self.linkage_rule_violation)
    }
}

/// `(class, name, descriptor)` of `Fieldref` / `Methodref` /
/// `InterfaceMethodref` `index`; `None` for anything else.
pub(crate) fn member_ref_parts(cp: &ConstantPool, index: u16) -> Option<(&str, &str, &str)> {
    let (class_index, name_and_type_index) = match cp.get(index)? {
        ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let class = cp.get_class_name(class_index)?;
    let (name, descriptor) = cp.get_name_and_type(name_and_type_index)?;
    Some((class, name, descriptor))
}

/// JVMS §4.10.1.8, HotSpot `ClassVerifier::verify_field_instructions`
/// (`check_protected`) and `verify_invoke_instructions` (`invokevirtual`):
/// the objectref `receiver` of a `getfield` / `putfield` / `invokevirtual`
/// of a protected member of a proper superclass declared in another run-time
/// package must be assignable to the current class.
///
/// HotSpot's split-verifier text is "Bad access to protected data in
/// getfield" for BOTH field opcodes (one shared `check_protected` label) and
/// "... in invokevirtual"; the old verifier's (major <= 50) is "Bad access to
/// protected data". A `null` receiver is assignable; an array receiver of
/// `Object.clone()` is exempt (arrays pretend to implement a public
/// `clone`). An unloaded receiver class answers "assignable" through the
/// hierarchy's optimism (HotSpot would load it), so no false refusal.
#[allow(clippy::too_many_arguments)]
fn check_protected_receiver(
    scratch: &mut VerifyScratch,
    cp: &ConstantPool,
    index: u16,
    receiver: &VType,
    current: &str,
    hierarchy: &dyn ClassHierarchy,
    opcode: &str,
    is_method: bool,
) -> Result<(), LinkageError> {
    if scratch.skip_linkage_rules {
        return Ok(());
    }
    let receiver_class: Option<&str> = match receiver {
        // `_this_type == stack_object_type`: nothing to check.
        VType::ObjectRef(name) if &**name == current => return Ok(()),
        VType::ObjectRef(name) => Some(&**name),
        VType::ArrayRef(_) => None,
        // `null` is assignable; uninitialized receivers and dead (`Top`)
        // slots are other rules' business.
        _ => return Ok(()),
    };
    let Some((ref_class, name, descriptor)) = member_ref_parts(cp, index) else {
        return Ok(());
    };
    let applies = match scratch.protected_rule.get(&index) {
        Some(&applies) => applies,
        None => {
            let applies = linkage_rule_protected_candidate(
                ref_class, name, descriptor, current, hierarchy, is_method,
            );
            scratch.protected_rule.insert(index, applies);
            applies
        }
    };
    if !applies {
        return Ok(());
    }
    let assignable = match receiver_class {
        // A receiver not even assignable to the referenced class is a type
        // the walk could not pin down (an inference merge over unloaded
        // classes falls back to a coarse common supertype) or code HotSpot
        // refuses with another message ("Bad type on operand stack ...")
        // that this verifier does not make: no refusal from this rule.
        Some(class) => {
            hierarchy.is_subclass(class, current) || !hierarchy.is_subclass(class, ref_class)
        }
        None => is_method && ref_class == "java/lang/Object" && name == "clone",
    };
    if assignable {
        return Ok(());
    }
    scratch.linkage_rule_violation = true;
    let message = if scratch.old_texts {
        "Bad access to protected data".to_string()
    } else {
        format!("Bad access to protected data in {opcode}")
    };
    Err(verify_err(&message))
}

/// HotSpot's receiver rule (interpreter round i5 wave 101, lane L7;
/// `i101-L7-verifier-accepts-unassignable-receivers`): the objectref of an
/// `invokevirtual` / `getfield` / `putfield` is popped as the referenced class
/// `target`, and a non-`<init>` `invokespecial`'s as the current class
/// (`verify_field_instructions`, `verify_invoke_instructions`). An
/// unassignable receiver is "Bad type on operand stack" (`header`; HotSpot's
/// `putfield` says "... in putfield" and names the expected type's constant
/// pool entry, `cp_origin`).
///
/// * `java/lang/Object` and interface targets accept any reference (the
///   verifier treats interfaces as `Object`; `is_assignable_to` already
///   relaxes them).
/// * `null` is assignable. An uninitialized receiver is left to the
///   `<init>` rules this walk already has (HotSpot's `putfield` on
///   `uninitializedThis` of a field of the current class is legal; the other
///   uses are refused elsewhere or not at all, as before).
/// * HotSpot LOADS a class to answer; this verifier cannot load from inside
///   a define (loader constraints, circularity), so a pair it cannot decide
///   because either class is not loaded yet is accepted, as before this rule
///   existed. The assignability query itself records HotSpot's verifier
///   loads (`note_hotspot_assignability_loads`), replayed at link.
/// * Answers are cached per class walk in `scratch.receiver_assignable`.
#[allow(clippy::too_many_arguments)]
fn check_receiver_assignable(
    frame: &mut VerificationFrame,
    scratch: &mut VerifyScratch,
    receiver: &VType,
    target: &str,
    hierarchy: &dyn ClassHierarchy,
    header: &str,
    what: &str,
    expected_cp: Option<u16>,
) -> Result<(), LinkageError> {
    if !(scratch.receiver_rule || scratch.old_receiver_rule_on)
        || scratch.skip_linkage_rules
        || target == "java/lang/Object"
    {
        return Ok(());
    }
    let from: &Arc<str> = match receiver {
        VType::ObjectRef(name) | VType::ArrayRef(name) => name,
        // `null`, uninitialized receivers and non-references: other rules.
        _ => return Ok(()),
    };
    if **from == *target {
        return Ok(());
    }
    // A merged value that fell back over a class not loaded (per VALUE:
    // the hierarchy recognises the `Arc` it handed out; interpreter round i5
    // wave 112, lane L7) is accepted; every other receiver, `Object` and
    // arrays included, is a libverify-exact merge or a producer's type.
    if !scratch.receiver_rule && scratch.old_receiver_merged && hierarchy.is_fallback_merge(from)
    {
        return Ok(());
    }
    let cached = scratch
        .receiver_assignable
        .get(target)
        .and_then(|m| m.get(&**from))
        .copied();
    let ok = match cached {
        Some(ok) => ok,
        None => {
            let expected = if target.starts_with('[') {
                VType::ArrayRef(Arc::from(target))
            } else {
                VType::ObjectRef(Arc::from(target))
            };
            let element = |n: &str| -> String {
                n.trim_start_matches('[')
                    .strip_prefix('L')
                    .and_then(|s| s.strip_suffix(';'))
                    .unwrap_or(n)
                    .to_string()
            };
            let decidable = hierarchy.is_resolvable(&element(target))
                && hierarchy.is_resolvable(&element(from));
            let ok = receiver.is_assignable_to(&expected, hierarchy) || !decidable;
            scratch
                .receiver_assignable
                .entry(Arc::from(target))
                .or_default()
                .insert(Arc::clone(from), ok);
            ok
        }
    };
    if ok {
        return Ok(());
    }
    let index = frame.stack.len();
    let expected = if target.starts_with('[') {
        VType::ArrayRef(Arc::from(target))
    } else {
        VType::ObjectRef(Arc::from(target))
    };
    frame.set_fault(VerifyFault::BadTypeWith {
        header: header.to_string(),
        index,
        actual: HsType::V(receiver.clone()),
        expected: HsType::V(expected),
        expected_cp,
    });
    Err(verify_err(&format!(
        "{what}: receiver {receiver:?} is not assignable to {target}"
    )))
}

/// [`check_protected_receiver`], recording HotSpot's fault for the refusal
/// (`bad_type(bci, stack_top_ctx(), implicit(current_type()))` under "Bad
/// access to protected data in <opcode>"; interpreter round i5 wave 101,
/// lane L7).
#[allow(clippy::too_many_arguments)]
fn check_protected_receiver_in(
    frame: &mut VerificationFrame,
    scratch: &mut VerifyScratch,
    cp: &ConstantPool,
    index: u16,
    receiver: &VType,
    current: &str,
    hierarchy: &dyn ClassHierarchy,
    opcode: &str,
    is_method: bool,
) -> Result<(), LinkageError> {
    let result = check_protected_receiver(
        scratch,
        cp,
        index,
        receiver,
        current,
        hierarchy,
        opcode,
        is_method,
    );
    // Recorded for libverify's texts too: `verify_old_text::post_pop_reason`
    // reads the "protected" header to say "Bad access to protected data"
    // (interpreter round i5 wave 109, lane L7: without the fault an old
    // class's refusal fell to "Incompatible type for getting or setting
    // field" or this VM's own text).
    if result.is_err() {
        let index = frame.stack.len();
        frame.set_fault(VerifyFault::BadTypeWith {
            header: format!("Bad access to protected data in {opcode}"),
            index,
            actual: HsType::V(receiver.clone()),
            expected: HsType::V(VType::ObjectRef(Arc::from(current))),
            expected_cp: None,
        });
    }
    result
}

/// The receiver-independent half of [`check_protected_receiver`]: does the
/// protected rule apply to a member reference through `ref_class` at all?
fn linkage_rule_protected_candidate(
    ref_class: &str,
    name: &str,
    descriptor: &str,
    current: &str,
    hierarchy: &dyn ClassHierarchy,
    is_method: bool,
) -> bool {
    // `name_in_supers` names a PROPER superclass.
    ref_class != current
        && !ref_class.starts_with('[')
        && hierarchy.protected_receiver_rule(current, ref_class, name, descriptor, is_method)
}

/// JVMS §4.10.1.9, HotSpot `ClassVerifier::verify_invoke_instructions`: a
/// non-`<init>` `invokespecial` must name the current class, its direct
/// superclass, a direct superinterface, or a class the current class is a
/// subclass of; an `InterfaceMethodref` must not name an indirect
/// superinterface. The old verifier (major <= 50) has only the first rule,
/// as "Illegal use of nonvirtual function call".
fn check_invokespecial_target(
    scratch: &mut VerifyScratch,
    cp: &ConstantPool,
    index: u16,
    current: &str,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    if scratch.skip_linkage_rules {
        return Ok(());
    }
    let Some(message) =
        invokespecial_target_refusal(
            cp,
            index,
            current,
            hierarchy,
            scratch.old_verifier,
            scratch.old_texts,
        )
    else {
        return Ok(());
    };
    scratch.linkage_rule_violation = true;
    Err(verify_err(message))
}

/// [`check_invokespecial_target`], recording HotSpot's `bad_code(bci)` fault
/// under the refusal's text (interpreter round i5 wave 101, lane L7).
fn check_invokespecial_target_in(
    frame: &mut VerificationFrame,
    scratch: &mut VerifyScratch,
    cp: &ConstantPool,
    index: u16,
    current: &str,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    let result = check_invokespecial_target(scratch, cp, index, current, hierarchy);
    if let Err(LinkageError::VerifyError { message, .. }) = &result {
        frame.set_fault(VerifyFault::BadCode {
            header: message.clone(),
        });
    }
    result
}

/// HotSpot `verify_invoke_instructions`: "Make sure <init> can only be
/// invoked by invokespecial" -- a method reference whose name starts with `<`
/// is legal only as `invokespecial` of `<init>`; anything else
/// (`invokevirtual` / `invokestatic` / `invokeinterface` of `<init>`, any
/// invoke of `<clinit>`) is `bad_code` "Illegal call to internal method".
/// Interpreter round i5 wave 107, lane L7 (the rule was missing: such a call
/// verified and failed at run time instead).
fn check_internal_method_call(
    frame: &mut VerificationFrame,
    info: &MethodRefInfo,
    invokespecial: bool,
) -> Result<(), LinkageError> {
    if !info.special_name || (invokespecial && info.is_init) {
        return Ok(());
    }
    frame.set_fault(VerifyFault::BadCode {
        header: "Illegal call to internal method".to_string(),
    });
    Err(verify_err("Illegal call to internal method"))
}

/// The message [`check_invokespecial_target`] refuses `index` with, if any.
fn invokespecial_target_refusal(
    cp: &ConstantPool,
    index: u16,
    current: &str,
    hierarchy: &dyn ClassHierarchy,
    old_verifier: bool,
    old_texts: bool,
) -> Option<&'static str> {
    let (ref_class, name, _) = member_ref_parts(cp, index)?;
    if name == "<init>" || ref_class == current {
        return None;
    }
    let interface_ref = matches!(
        cp.get(index),
        Some(ConstantPoolEntry::InterfaceMethodReference { .. })
    );
    match hierarchy.invokespecial_target(current, ref_class)? {
        super::vtype::SpecialTarget::SameOrDirect => None,
        super::vtype::SpecialTarget::NotAssignable => Some(if old_texts {
            "Illegal use of nonvirtual function call"
        } else {
            "Bad invokespecial instruction: current class isn't assignable to reference class."
        }),
        super::vtype::SpecialTarget::Assignable if interface_ref && !old_verifier => Some(
            "Bad invokespecial instruction: interface method to invoke is not in a direct \
             superinterface.",
        ),
        super::vtype::SpecialTarget::Assignable => None,
    }
}

/// Could `insn` be refused by a linkage rule ([`check_protected_receiver`],
/// [`check_invokespecial_target`]) in class `current`? Decided without the
/// type state (the protected rule's receiver test is left to the walk), so a
/// `false` is final. For the define-time pre-scan of a class whose Pass-3
/// verdict is deferred (`verifier::verify_deferred_class_bytecode`).
pub(crate) fn linkage_rule_candidate(
    insn: &Instruction,
    cp: &ConstantPool,
    current: &str,
    hierarchy: &dyn ClassHierarchy,
    old_verifier: bool,
) -> bool {
    let (index, is_method) = match insn {
        Instruction::Getfield(index) | Instruction::Putfield(index) => (*index, false),
        Instruction::Invokevirtual(index) => (*index, true),
        Instruction::Invokespecial(index) => {
            // A constructor call: `verify_invoke_init`'s protected rule
            // (interpreter round i5 wave 108, lane L7). The scratch applies it
            // from major 50; a candidate below is only a wasted walk.
            if let Some((ref_class, "<init>", descriptor)) = member_ref_parts(cp, *index) {
                return linkage_rule_protected_candidate(
                    ref_class, "<init>", descriptor, current, hierarchy, true,
                );
            }
            return invokespecial_target_refusal(
                cp,
                *index,
                current,
                hierarchy,
                old_verifier,
                old_verifier,
            )
            .is_some();
        }
        _ => return false,
    };
    member_ref_parts(cp, index).is_some_and(|(ref_class, name, descriptor)| {
        linkage_rule_protected_candidate(ref_class, name, descriptor, current, hierarchy, is_method)
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Method,
    InterfaceMethod,
    Dynamic,
}

/// Which constant pool entry kinds an invoke opcode accepts.
#[derive(Clone, Copy)]
enum RefAccept {
    /// `invokevirtual`: a `Methodref`.
    Method,
    /// `invokespecial` / `invokestatic`: a `Methodref` or `InterfaceMethodref`.
    MethodOrInterface,
    /// `invokeinterface`: an `InterfaceMethodref`.
    Interface,
    /// `invokedynamic`: an `InvokeDynamic`.
    Dynamic,
}

impl RefAccept {
    fn allows(self, kind: RefKind) -> bool {
        match self {
            RefAccept::Method => kind == RefKind::Method,
            RefAccept::MethodOrInterface => {
                kind == RefKind::Method || kind == RefKind::InterfaceMethod
            }
            RefAccept::Interface => kind == RefKind::InterfaceMethod,
            RefAccept::Dynamic => kind == RefKind::Dynamic,
        }
    }
}

/// A method descriptor, validated and parsed: the parameter types and the
/// return type (`None` for `void`).
struct ParsedSig {
    /// `param_types_from_descriptor(descriptor)`.
    params: Vec<VType>,
    /// `return_type_from_descriptor(descriptor)`.
    ret: Option<VType>,
}

/// Distinct descriptors a thread has parsed. A class file's descriptors repeat
/// across its call sites and, far more, across classes (`()V`, `(I)V`,
/// `(Ljava/lang/Object;)Z`, ...): parsing one allocates a `Vec` and an
/// `Arc<str>` per object-typed parameter, so each distinct one is parsed once
/// per thread. Bounded: the map is dropped when it reaches `MAX_SIG_CACHE`.
const MAX_SIG_CACHE: usize = 8192;

thread_local! {
    static SIG_CACHE: std::cell::RefCell<rustc_hash::FxHashMap<Box<str>, std::rc::Rc<ParsedSig>>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// The parsed form of method descriptor `descriptor`, or `None` when it is not
/// a well-formed method descriptor (JVMS §4.3.3).
fn parsed_sig(descriptor: &str) -> Option<std::rc::Rc<ParsedSig>> {
    SIG_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(sig) = cache.get(descriptor) {
            return Some(std::rc::Rc::clone(sig));
        }
        if !super::vtype::is_valid_method_descriptor(descriptor) {
            return None;
        }
        let sig = std::rc::Rc::new(ParsedSig {
            params: super::vtype::param_types_from_descriptor(descriptor),
            ret: return_type_from_descriptor(descriptor),
        });
        if cache.len() >= MAX_SIG_CACHE {
            cache.clear();
        }
        cache.insert(Box::from(descriptor), std::rc::Rc::clone(&sig));
        Some(sig)
    })
}

/// Distinct field descriptors a thread has turned into verification types
/// (same reasoning as [`SIG_CACHE`]: `Ljava/lang/String;` is allocated into an
/// `Arc<str>` on every first use otherwise).
thread_local! {
    static FIELD_TYPE_CACHE: std::cell::RefCell<rustc_hash::FxHashMap<Box<str>, VType>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `VType::from_field_descriptor(descriptor)` for a well-formed field
/// descriptor (JVMS §4.3.2), through the per-thread cache; `None` when it is
/// not well-formed. Only validated descriptors are ever cached, so a cache hit
/// needs no validation walk.
fn field_vtype(descriptor: &str) -> Option<VType> {
    FIELD_TYPE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(t) = cache.get(descriptor) {
            return Some(t.clone());
        }
        if !super::vtype::is_valid_field_descriptor(descriptor) {
            return None;
        }
        let t = VType::from_field_descriptor(descriptor);
        if cache.len() >= MAX_SIG_CACHE {
            cache.clear();
        }
        cache.insert(Box::from(descriptor), t.clone());
        Some(t)
    })
}

/// A resolved, validated method reference (what an invoke opcode needs).
struct MethodRefInfo {
    kind: RefKind,
    /// The parsed descriptor (shared with every other reference to a method of
    /// the same descriptor).
    sig: std::rc::Rc<ParsedSig>,
    /// The method's name is `<init>`.
    is_init: bool,
    /// The method's name starts with `<` (`<init>` or `<clinit>`): only
    /// `invokespecial` of `<init>` may name one ("Illegal call to internal
    /// method"; interpreter round i5 wave 107, lane L7).
    special_name: bool,
    /// The declaring class, for a `Methodref` (what
    /// `resolve_method_owner_name_and_type` yields); `None` for the other kinds.
    owner: Option<Arc<str>>,
    /// The method name when this is `Reference.get0` / `refersTo0` /
    /// `clear0` (the receiver check `invokevirtual` makes for them).
    reference_special: Option<String>,
}

/// Resolve invoke operand `index` for an opcode that accepts `accept`.
fn method_info(
    scratch: &mut VerifyScratch,
    cp: &ConstantPool,
    index: u16,
    accept: RefAccept,
) -> Result<std::rc::Rc<MethodRefInfo>, LinkageError> {
    if let Some(info) = scratch.methods.get(&index) {
        if accept.allows(info.kind) {
            return Ok(std::rc::Rc::clone(info));
        }
    }
    // Not seen yet (or seen as a kind this opcode refuses). The common case is
    // a well-formed reference of an accepted kind, resolved here without
    // copying anything; anything else is handed to the resolution code that
    // always did the job, so a bad operand is rejected with the same message.
    if let Some(info) = method_info_fast(cp, index, accept) {
        scratch.methods.insert(index, std::rc::Rc::clone(&info));
        return Ok(info);
    }
    let (name, descriptor) = match accept {
        RefAccept::Method => resolve_method_name_and_type(cp, index)?,
        RefAccept::MethodOrInterface => resolve_method_or_imethod_name_and_type(cp, index)?,
        RefAccept::Interface => resolve_imethod_name_and_type(cp, index)?,
        RefAccept::Dynamic => (String::new(), resolve_invokedynamic_type(cp, index)?),
    };
    let (kind, owner) = match cp.get(index) {
        Some(ConstantPoolEntry::MethodReference { class_index, .. }) => {
            (RefKind::Method, cp.get_class_name_arc(*class_index))
        }
        Some(ConstantPoolEntry::InterfaceMethodReference { .. }) => {
            (RefKind::InterfaceMethod, None)
        }
        _ => (RefKind::Dynamic, None),
    };
    let reference_special = reference_special_name(owner.as_deref(), kind, &name);
    let Some(sig) = parsed_sig(&descriptor) else {
        // Unreachable: the resolution above only succeeds for a well-formed
        // descriptor. Fail closed all the same.
        return Err(verify_err(&format!(
            "method ref: {descriptor:?} is not a well-formed method descriptor (JVMS §4.3.3)"
        )));
    };
    let info = std::rc::Rc::new(MethodRefInfo {
        kind,
        sig,
        is_init: name == "<init>",
        special_name: kind != RefKind::Dynamic && name.starts_with('<'),
        owner,
        reference_special,
    });
    scratch.methods.insert(index, std::rc::Rc::clone(&info));
    Ok(info)
}

/// The method name when the reference is to `Reference.get0` / `refersTo0` /
/// `clear0` (a `Methodref` whose owner is `java/lang/ref/Reference`).
fn reference_special_name(owner: Option<&str>, kind: RefKind, name: &str) -> Option<String> {
    match (owner, kind) {
        (Some(owner), RefKind::Method)
            if owner == "java/lang/ref/Reference"
                && (name == "get0" || name == "refersTo0" || name == "clear0") =>
        {
            Some(name.to_string())
        }
        _ => None,
    }
}

/// [`method_info`]'s resolution of a well-formed reference of a kind `accept`
/// allows: the same checks as `checked_name_and_type` (a non-empty name and a
/// well-formed method descriptor, via [`parsed_sig`]) on borrowed strings.
/// `None` for anything else, which the caller resolves the slow way for the
/// error.
fn method_info_fast(
    cp: &ConstantPool,
    index: u16,
    accept: RefAccept,
) -> Option<std::rc::Rc<MethodRefInfo>> {
    let (kind, class_index, name_and_type_index) = match cp.get(index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        } => (RefKind::Method, Some(*class_index), *name_and_type_index),
        ConstantPoolEntry::InterfaceMethodReference {
            name_and_type_index,
            ..
        } => (RefKind::InterfaceMethod, None, *name_and_type_index),
        ConstantPoolEntry::InvokeDynamic {
            name_and_type_index,
            ..
        } => (RefKind::Dynamic, None, *name_and_type_index),
        _ => return None,
    };
    if !accept.allows(kind) {
        return None;
    }
    let (name, descriptor) = cp.get_name_and_type(name_and_type_index)?;
    if name.is_empty() {
        return None;
    }
    let sig = parsed_sig(descriptor)?;
    let owner = class_index.and_then(|ci| cp.get_class_name_arc(ci));
    let reference_special = reference_special_name(owner.as_deref(), kind, name);
    Some(std::rc::Rc::new(MethodRefInfo {
        kind,
        sig,
        // (`invokedynamic`'s name is never consulted, and `<init>` is not a
        // legal dynamic call site name; the slow path records the same thing.)
        is_init: kind != RefKind::Dynamic && name == "<init>",
        special_name: kind != RefKind::Dynamic && name.starts_with('<'),
        owner,
        reference_special,
    }))
}

/// The type of `Fieldref` `index` (validated), memoised in `scratch`.
fn field_type_of(
    scratch: &mut VerifyScratch,
    cp: &ConstantPool,
    index: u16,
) -> Result<VType, LinkageError> {
    if let Some(t) = scratch.fields.get(&index) {
        return Ok(t.clone());
    }
    let t = resolve_field_type(cp, index)?;
    scratch.fields.insert(index, t.clone());
    Ok(t)
}

/// [`verify_instruction`] with a caller-held [`VerifyScratch`] for the constant
/// pool of the class being verified.
#[allow(clippy::too_many_arguments)]
pub fn verify_instruction_with(
    insn: &Instruction,
    pc: usize,
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    _class_name: &str,
    _method_name: &str,
    method_descriptor: &str,
    hierarchy: &dyn ClassHierarchy,
    scratch: &mut VerifyScratch,
) -> Result<InsnVerifyResult, LinkageError> {
    let current_class_name = _class_name;
    scratch.old_receiver_rule_on = scratch.old_receiver_method != (0, 0)
        && scratch.old_receiver_method
            == (_method_name.as_ptr() as usize, method_descriptor.as_ptr() as usize);
    match insn {
        // =====================================================================
        // Constants — push known types
        // =====================================================================
        Instruction::Nop => ok_through(),

        Instruction::AconstNull => {
            frame.push(VType::Null)?;
            ok_through()
        }

        Instruction::IconstM1
        | Instruction::Iconst0
        | Instruction::Iconst1
        | Instruction::Iconst2
        | Instruction::Iconst3
        | Instruction::Iconst4
        | Instruction::Iconst5
        | Instruction::Bipush(_)
        | Instruction::Sipush(_) => {
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Lconst0 | Instruction::Lconst1 => {
            frame.push(VType::Long)?;
            frame.push(VType::Top)?; // category-2 second slot
            ok_through()
        }

        Instruction::Fconst0 | Instruction::Fconst1 | Instruction::Fconst2 => {
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Dconst0 | Instruction::Dconst1 => {
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Ldc(index) => verify_ldc(
            frame,
            cp,
            *index as u16,
            current_class_name,
            scratch.ldc_class_refused,
        ),
        Instruction::LdcW(index) => {
            verify_ldc(frame, cp, *index, current_class_name, scratch.ldc_class_refused)
        }
        Instruction::Ldc2W(index) => verify_ldc2w(frame, cp, *index, current_class_name),

        // =====================================================================
        // Loads — read from local variable, push to stack
        // =====================================================================
        Instruction::Iload(index) => {
            let local = frame.local_load(*index)?;
            if !local.is_assignable_to(&VType::Int, hierarchy) {
                let message = format!("iload: local {index} is {local:?}, expected Int");
                set_local_fault(frame, *index, HsType::V(VType::Int));
                return Err(verify_err(&message));
            }
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Lload(index) => {
            // JVMS §4.10.1.6: the pair (`index`, `index+1`) must still be an
            // intact category-2 value. Checking only the base would accept a
            // pair whose upper half an intervening category-1 store replaced.
            if let Err(e) = frame.local_load_wide(*index, &VType::Long) {
                set_local_pair_fault(frame, *index, &VType::Long);
                return Err(prefix_verify_err("lload", e));
            }
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Fload(index) => {
            let local = frame.local_load(*index)?;
            if !local.is_assignable_to(&VType::Float, hierarchy) {
                let message = format!("fload: local {index} is {local:?}, expected Float");
                set_local_fault(frame, *index, HsType::V(VType::Float));
                return Err(verify_err(&message));
            }
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Dload(index) => {
            if let Err(e) = frame.local_load_wide(*index, &VType::Double) {
                set_local_pair_fault(frame, *index, &VType::Double);
                return Err(prefix_verify_err("dload", e));
            }
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Aload(index) => {
            let local = frame.local_load(*index)?.clone();
            if !local.is_reference() && local != VType::ReturnAddress(0) {
                // ReturnAddress is also loadable by aload (for jsr/ret)
                // We need a more permissive check for ReturnAddress
                match &local {
                    VType::ReturnAddress(_) => {}
                    _ if !local.is_reference() => {
                        set_local_fault(frame, *index, HsType::Reference);
                        return Err(verify_err(&format!(
                            "aload: local {index} is {local:?}, expected reference"
                        )));
                    }
                    _ => {}
                }
            }
            frame.push(local)?;
            ok_through()
        }

        // Array loads: pop index (int), pop arrayref, push element type
        Instruction::Iaload | Instruction::Baload | Instruction::Caload | Instruction::Saload => {
            frame.pop_expect(&VType::Int, hierarchy)?; // index
            let (op, kind) = match insn {
                Instruction::Iaload => ("iaload", ArrayKind::Of("[I")),
                Instruction::Baload => ("baload", ArrayKind::ByteOrBool),
                Instruction::Caload => ("caload", ArrayKind::Of("[C")),
                _ => ("saload", ArrayKind::Of("[S")),
            };
            pop_array_of(frame, hierarchy, op, kind)?; // arrayref
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Laload => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_of(frame, hierarchy, "laload", ArrayKind::Of("[J"))?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Faload => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_of(frame, hierarchy, "faload", ArrayKind::Of("[F"))?;
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Daload => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_of(frame, hierarchy, "daload", ArrayKind::Of("[D"))?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Aaload => {
            frame.pop_expect(&VType::Int, hierarchy)?; // index
            // HotSpot: `reference_check()`, then an array of references
            // ("Bad type on operand stack in aaload"). This used to take ANY
            // value (an int, a `String`) and push `Object` (interpreter
            // round i5 wave 102, lane L7).
            let arrayref = pop_array_of(frame, hierarchy, "aaload", ArrayKind::References)?;
                                         // Determine element type from array descriptor
            let elem_type = match &arrayref {
                VType::ArrayRef(desc) => {
                    let elem_desc = &desc[1..];
                    // (cached for a well-formed element descriptor; anything
                    // else keeps the permissive original conversion)
                    field_vtype(elem_desc)
                        .unwrap_or_else(|| VType::from_field_descriptor(elem_desc))
                }
                VType::Null => VType::Null,
                _ => VType::ObjectRef(with_well_known(|w| Arc::clone(&w.object))),
            };
            frame.push(elem_type)?;
            ok_through()
        }

        // =====================================================================
        // Stores — pop from stack, store to local variable
        // =====================================================================
        Instruction::Istore(index) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.local_store(*index, VType::Int)?;
            ok_through()
        }

        Instruction::Lstore(index) => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            // `local_store_wide` bounds-checks BOTH slots against `max_locals`
            // and computes `index + 1` with `checked_add` — the previous
            // `*index + 1` overflowed for a `wide lstore 65535`.
            frame
                .local_store_wide(*index, VType::Long)
                .map_err(|e| prefix_verify_err("lstore", e))?;
            ok_through()
        }

        Instruction::Fstore(index) => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.local_store(*index, VType::Float)?;
            ok_through()
        }

        Instruction::Dstore(index) => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame
                .local_store_wide(*index, VType::Double)
                .map_err(|e| prefix_verify_err("dstore", e))?;
            ok_through()
        }

        Instruction::Astore(index) => {
            // HotSpot `verify_astore`: `pop_stack(reference_check())`, "Bad
            // type on operand stack" ... "is not assignable to reference type"
            // (interpreter round i5 wave 109, lane L7: no fault was recorded,
            // so the refusal kept this VM's text).
            let n = frame.stack.len();
            let actual = n.checked_sub(1).map(|i| VerifyFault::slot_type(&frame.stack, i));
            let val = frame.pop()?;
            if !val.is_reference() && !matches!(val, VType::ReturnAddress(_)) {
                if let Some(actual) = actual {
                    frame.set_fault(VerifyFault::BadStackType {
                        index: n - 1,
                        actual,
                        expected: HsType::Reference,
                    });
                }
                return Err(verify_err(&format!(
                    "astore: expected reference, found {val:?}"
                )));
            }
            frame.local_store(*index, val)?;
            ok_through()
        }

        // Array stores: pop value, pop index, pop arrayref
        Instruction::Iastore
        | Instruction::Bastore
        | Instruction::Castore
        | Instruction::Sastore => {
            frame.pop_expect(&VType::Int, hierarchy)?; // value
            frame.pop_expect(&VType::Int, hierarchy)?; // index
            let (op, kind) = match insn {
                Instruction::Iastore => ("iastore", ArrayKind::Of("[I")),
                Instruction::Bastore => ("bastore", ArrayKind::ByteOrBool),
                Instruction::Castore => ("castore", ArrayKind::Of("[C")),
                _ => ("sastore", ArrayKind::Of("[S")),
            };
            pop_array_of(frame, hierarchy, op, kind)?; // arrayref
            ok_through()
        }

        Instruction::Lastore => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_of(frame, hierarchy, "lastore", ArrayKind::Of("[J"))?;
            ok_through()
        }

        Instruction::Fastore => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_of(frame, hierarchy, "fastore", ArrayKind::Of("[F"))?;
            ok_through()
        }

        Instruction::Dastore => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            pop_array_of(frame, hierarchy, "dastore", ArrayKind::Of("[D"))?;
            ok_through()
        }

        Instruction::Aastore => {
            // T1.3.5 — verify the value is a reference and (when the
            // array's element type is statically known) is
            // assignment-compatible with the element type.
            //
            // Per JVMS §6.5 aastore: "the value must be of a type
            // that is assignment compatible with the component type
            // of the array". Full assignment compatibility against
            // arbitrary class hierarchies is checked at runtime to
            // raise `ArrayStoreException`, but the verifier rejects
            // anything that isn't a reference at all.
            // HotSpot pops the value as `java/lang/Object` (an uninitialized
            // object is not one), the index, then the arrayref with
            // `reference_check()` and requires an array of references
            // (interpreter round i5 wave 102, lane L7: the faults).
            let value = pop_reference_or(frame, |index, actual| VerifyFault::BadStackType {
                index,
                actual,
                expected: HsType::V(object_vtype()),
            })?;
            if matches!(value, VType::Uninitialized(_) | VType::UninitializedThis) {
                let index = frame.stack.len();
                frame.set_fault(VerifyFault::BadStackType {
                    index,
                    actual: HsType::V(value.clone()),
                    expected: HsType::V(object_vtype()),
                });
                return Err(verify_err(&format!(
                    "aastore: expected an initialized reference value, found {value:?}"
                )));
            }
            frame.pop_expect(&VType::Int, hierarchy)?; // index
            // Null is allowed (it NPEs at run time, the spec'd outcome).
            pop_array_of(frame, hierarchy, "aastore", ArrayKind::References)?;
            ok_through()
        }

        // =====================================================================
        // Stack manipulation
        // =====================================================================
        Instruction::Pop => {
            // Operates on a single slot. The top slot must be a complete
            // category-1 value — `Top` sitting above a `Long`/`Double` base
            // is the upper half of a category-2 value and may not be split.
            let val = frame.pop()?;
            if is_cat2_upper_half(frame, &val) {
                return Err(verify_err("pop: cannot pop category-2 value with pop"));
            }
            ok_through()
        }

        Instruction::Pop2 => {
            // Pops the top two slots: either two category-1 values, or one
            // category-2 value (`Long`/`Double` base + `Top`).
            let slots = pop_n_slots(frame, 2, "pop2")?;
            check_no_split(&slots, "pop2")?;
            ok_through()
        }

        Instruction::Dup => {
            // Duplicate the top single slot. The top slot must be a complete
            // category-1 value.
            let val = frame.pop()?;
            if is_cat2_upper_half(frame, &val) {
                return Err(verify_err("dup: cannot dup category-2 value"));
            }
            frame.push(val.clone())?;
            frame.push(val)?;
            ok_through()
        }

        Instruction::DupX1 => {
            // ..., v2, v1 -> ..., v1, v2, v1   (both single category-1 slots)
            let val1 = frame.pop()?;
            let val2 = frame.pop()?;
            if is_cat2_upper_half(frame, &val2) || val1 == VType::Top || val2 == VType::Top {
                return Err(verify_err("dup_x1: category-2 value not allowed"));
            }
            frame.push(val1.clone())?;
            frame.push(val2)?;
            frame.push(val1)?;
            ok_through()
        }

        Instruction::DupX2 => {
            // ..., {v3,v2}, v1 -> ..., v1, {v3,v2}, v1
            // v1 is a single category-1 slot; {v3,v2} is the next two slots
            // (either two category-1, or one category-2).
            let val1 = frame.pop()?;
            if is_cat2_upper_half(frame, &val1) || val1 == VType::Top {
                return Err(verify_err("dup_x2: val1 must be category-1"));
            }
            let lower = pop_n_slots(frame, 2, "dup_x2")?;
            check_no_split(&lower, "dup_x2")?;
            frame.push(val1.clone())?;
            for v in &lower {
                frame.push(v.clone())?;
            }
            frame.push(val1)?;
            ok_through()
        }

        Instruction::Dup2 => {
            // Duplicate the top two slots (either two category-1 values, or
            // one category-2 value).
            let slots = pop_n_slots(frame, 2, "dup2")?;
            check_no_split(&slots, "dup2")?;
            for v in &slots {
                frame.push(v.clone())?;
            }
            for v in &slots {
                frame.push(v.clone())?;
            }
            ok_through()
        }

        Instruction::Dup2X1 => {
            // ..., v3, {v2,v1} -> ..., {v2,v1}, v3, {v2,v1}
            // top two slots ({v2,v1}) followed by one category-1 slot (v3).
            let top = pop_n_slots(frame, 2, "dup2_x1")?;
            check_no_split(&top, "dup2_x1")?;
            let val3 = frame.pop()?;
            if is_cat2_upper_half(frame, &val3) || val3 == VType::Top {
                return Err(verify_err("dup2_x1: third value must be category-1"));
            }
            for v in &top {
                frame.push(v.clone())?;
            }
            frame.push(val3)?;
            for v in &top {
                frame.push(v.clone())?;
            }
            ok_through()
        }

        Instruction::Dup2X2 => {
            // ..., {v4,v3}, {v2,v1} -> ..., {v2,v1}, {v4,v3}, {v2,v1}
            // top two slots followed by next two slots; neither pair may be
            // split across a category-2 boundary.
            let top = pop_n_slots(frame, 2, "dup2_x2")?;
            check_no_split(&top, "dup2_x2")?;
            let lower = pop_n_slots(frame, 2, "dup2_x2")?;
            check_no_split(&lower, "dup2_x2")?;
            for v in &top {
                frame.push(v.clone())?;
            }
            for v in &lower {
                frame.push(v.clone())?;
            }
            for v in &top {
                frame.push(v.clone())?;
            }
            ok_through()
        }

        Instruction::Swap => {
            let val1 = frame.pop()?;
            let val2 = frame.pop()?;
            if val1 == VType::Top || val2 == VType::Top || is_cat2_upper_half(frame, &val2) {
                return Err(verify_err("swap: cannot swap category-2 values"));
            }
            frame.push(val1)?;
            frame.push(val2)?;
            ok_through()
        }

        // =====================================================================
        // Arithmetic — pop operands, push result (same type)
        // =====================================================================
        Instruction::Iadd
        | Instruction::Isub
        | Instruction::Imul
        | Instruction::Idiv
        | Instruction::Irem
        | Instruction::Iand
        | Instruction::Ior
        | Instruction::Ixor => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Ladd
        | Instruction::Lsub
        | Instruction::Lmul
        | Instruction::Ldiv
        | Instruction::Lrem
        | Instruction::Land
        | Instruction::Lor
        | Instruction::Lxor => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Fadd
        | Instruction::Fsub
        | Instruction::Fmul
        | Instruction::Fdiv
        | Instruction::Frem => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Dadd
        | Instruction::Dsub
        | Instruction::Dmul
        | Instruction::Ddiv
        | Instruction::Drem => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Ineg => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Lneg => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Fneg => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }

        Instruction::Dneg => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        // Shifts: int operands, result is same type as first operand
        Instruction::Ishl | Instruction::Ishr | Instruction::Iushr => {
            frame.pop_expect(&VType::Int, hierarchy)?; // shift amount
            frame.pop_expect(&VType::Int, hierarchy)?; // value
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Lshl | Instruction::Lshr | Instruction::Lushr => {
            frame.pop_expect(&VType::Int, hierarchy)?; // shift amount (int)
            frame.pop_expect_cat2(&VType::Long, hierarchy)?; // value
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }

        Instruction::Iinc { index, .. } => {
            // iinc doesn't touch the stack; just verify the local is int
            let local = frame.local_load(*index)?;
            if !local.is_assignable_to(&VType::Int, hierarchy) {
                let message = format!("iinc: local {index} is {local:?}, expected Int");
                set_local_fault(frame, *index, HsType::V(VType::Int));
                return Err(verify_err(&message));
            }
            ok_through()
        }

        // =====================================================================
        // Conversions
        // =====================================================================
        Instruction::I2l => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::I2f => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }
        Instruction::I2d => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::L2i => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }
        Instruction::L2f => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }
        Instruction::L2d => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::F2i => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }
        Instruction::F2l => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::F2d => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::D2i => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }
        Instruction::D2l => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
            ok_through()
        }
        Instruction::D2f => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.push(VType::Float)?;
            ok_through()
        }
        Instruction::I2b | Instruction::I2c | Instruction::I2s => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        // =====================================================================
        // Comparisons — pop operands, push int result
        // =====================================================================
        Instruction::Lcmp => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Fcmpl | Instruction::Fcmpg => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.pop_expect(&VType::Float, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Dcmpl | Instruction::Dcmpg => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        // =====================================================================
        // Branches — pop operands, add branch target
        // =====================================================================
        Instruction::Ifeq(offset)
        | Instruction::Ifne(offset)
        | Instruction::Iflt(offset)
        | Instruction::Ifge(offset)
        | Instruction::Ifgt(offset)
        | Instruction::Ifle(offset) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: true,
                branch_targets: BranchTargets::one(target),
            })
        }

        Instruction::IfIcmpeq(offset)
        | Instruction::IfIcmpne(offset)
        | Instruction::IfIcmplt(offset)
        | Instruction::IfIcmpge(offset)
        | Instruction::IfIcmpgt(offset)
        | Instruction::IfIcmple(offset) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            frame.pop_expect(&VType::Int, hierarchy)?;
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: true,
                branch_targets: BranchTargets::one(target),
            })
        }

        Instruction::IfAcmpeq(offset) | Instruction::IfAcmpne(offset) => {
            pop_reference(frame, hierarchy)?;
            pop_reference(frame, hierarchy)?;
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: true,
                branch_targets: BranchTargets::one(target),
            })
        }

        Instruction::Ifnull(offset) | Instruction::Ifnonnull(offset) => {
            pop_reference(frame, hierarchy)?;
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: true,
                branch_targets: BranchTargets::one(target),
            })
        }

        // =====================================================================
        // Control — goto, switch, return
        // =====================================================================
        Instruction::Goto(offset) => {
            let target = branch_target(pc, *offset as i32);
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: BranchTargets::one(target),
            })
        }

        Instruction::GotoW(offset) => {
            let target = branch_target(pc, *offset);
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: BranchTargets::one(target),
            })
        }

        Instruction::Tableswitch(ts) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            let mut targets: Vec<u16> = ts
                .offsets
                .iter()
                .map(|off| branch_target(pc, *off))
                .collect();
            targets.push(branch_target(pc, ts.default));
            // Deduplicate
            targets.sort();
            targets.dedup();
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: targets.into(),
            })
        }

        Instruction::Lookupswitch(ls) => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            // HotSpot `verify_switch` (after the key pop): the match keys
            // must be strictly increasing, `bad_code` "Bad lookupswitch
            // instruction"; libverify's `verify_opcode_operands` says
            // "Unsorted lookup switch" (`verify_old_text`). The interpreter
            // would dispatch an unsorted table, so it verified here.
            // Interpreter round i5 wave 109, lane L7. A map-only walk keeps
            // its maps.
            if !scratch.skip_linkage_rules && ls.pairs.windows(2).any(|w| w[0].0 >= w[1].0) {
                frame.set_fault(VerifyFault::BadCode {
                    header: "Bad lookupswitch instruction".to_string(),
                });
                return Err(verify_err("Bad lookupswitch instruction"));
            }
            let mut targets: Vec<u16> = ls
                .pairs
                .iter()
                .map(|(_, off)| branch_target(pc, *off))
                .collect();
            targets.push(branch_target(pc, ls.default));
            targets.sort();
            targets.dedup();
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: targets.into(),
            })
        }

        // HotSpot's `verify_return_value` follows each pop (interpreter round
        // i5 wave 102, lane L7): these returns never looked at the method's
        // return type, so an `ireturn` from a `void` or `float` method was
        // accepted.
        Instruction::Ireturn => {
            frame.pop_expect(&VType::Int, hierarchy)?;
            let index = frame.stack.len();
            check_return_value(frame, &VType::Int, index, method_descriptor, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Lreturn => {
            frame.pop_expect_cat2(&VType::Long, hierarchy)?;
            let index = frame.stack.len();
            check_return_value(frame, &VType::Long, index, method_descriptor, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Freturn => {
            frame.pop_expect(&VType::Float, hierarchy)?;
            let index = frame.stack.len();
            check_return_value(frame, &VType::Float, index, method_descriptor, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Dreturn => {
            frame.pop_expect_cat2(&VType::Double, hierarchy)?;
            let index = frame.stack.len();
            check_return_value(frame, &VType::Double, index, method_descriptor, hierarchy)?;
            ok_no_fallthrough()
        }

        Instruction::Areturn => {
            // JVMS §4.10.1.6 / §6.5 areturn: the value popped must be a
            // reference that is assignable to the method's declared
            // return type. Popping any reference unchecked would let a
            // method declared to return type `T` actually return an
            // unrelated reference, breaking type safety for every caller.
            let value = pop_reference(frame, hierarchy)?;
            // HotSpot's `verify_return_value` texts (interpreter round i5
            // wave 102, lane L7): the value's slot, the declared type "(from
            // method signature)".
            let value_index = frame.stack.len();
            let declared_return = match parsed_sig(method_descriptor) {
                Some(sig) => sig.ret.clone(),
                None => return_type_from_descriptor(method_descriptor),
            };
            match declared_return {
                Some(declared @ (VType::ObjectRef(_) | VType::ArrayRef(_))) => {
                    // Loader-aware execution can hold distinct ClassIds for one
                    // binary type while Pass 3 frames retain only its binary
                    // name. That ambiguity is real, but it is already resolved
                    // upstream, not here: `ClassManager::define_class_with_options`
                    // defers this entire Pass 3 pass for `UserDefined`-loader
                    // classes (see `defer_loader_sensitive_pass3`), and
                    // `vm_util.rs::initialize_class_shared` does the same at
                    // link time. By the time this arm runs, the class being
                    // verified is NOT one of those deferred classes, so a
                    // mismatch here is a genuine violation, not a loader-name
                    // collision — reject it (JVMS §4.10.1.2 requires the
                    // verifier be conservative and reject what it cannot
                    // prove; see the RVERIF.3 note on `ClassHierarchy` in
                    // `vtype.rs` for the same principle applied to
                    // `is_assignable_to`).
                    if !value.is_assignable_to(&declared, hierarchy) {
                        frame.set_fault(VerifyFault::BadReturn {
                            header: "Bad return type".to_string(),
                            index: value_index,
                            actual: HsType::V(value.clone()),
                            expected: HsType::V(declared.clone()),
                        });
                        return Err(verify_err(&format!(
                            "areturn: returned value {value:?} is not assignable to \
                             the method's declared return type {declared:?}"
                        )));
                    }
                }
                // Declared return type is a primitive (Int/Long/Float/
                // Double) — `areturn` is the wrong return instruction.
                Some(other) => {
                    frame.set_fault(VerifyFault::BadReturn {
                        header: "Bad return type".to_string(),
                        index: value_index,
                        actual: HsType::V(value.clone()),
                        expected: HsType::V(other.clone()),
                    });
                    return Err(verify_err(&format!(
                        "areturn: method's declared return type {other:?} is not a \
                         reference type"
                    )));
                }
                // `None` means the method is declared `void` — `areturn`
                // must not be used to return from a void method.
                None => {
                    frame.set_fault(VerifyFault::BadReturn {
                        header: "Method does not expect a return value".to_string(),
                        index: value_index,
                        actual: HsType::V(value.clone()),
                        expected: HsType::V(VType::Top),
                    });
                    return Err(verify_err(
                        "areturn: method is declared void; cannot return a value",
                    ));
                }
            }
            ok_no_fallthrough()
        }

        Instruction::Return => {
            // JVMS §4.10.1.9 (`return`): if the method being verified is
            // `<init>` and the receiver has not been initialized — i.e. no
            // `invokespecial` of `this.<init>` / `super.<init>` has replaced
            // `uninitializedThis` — the constructor may not return. Otherwise
            // a class could hand its caller a fully-typed reference to an
            // object whose superclass constructor never ran, defeating every
            // invariant a superclass constructor establishes (this is the
            // classic "uninitialized object escape").
            //
            // `<init>` of `java/lang/Object` is the one exception: it has no
            // superclass to chain to, so its `uninitializedThis` is never
            // replaced.
            //
            // HotSpot first refuses a `return` from a method that declares a
            // return value: "Method expects a return value" (interpreter
            // round i5 wave 102, lane L7; `CRATONVM_VERIFY_CONTROL_RULES`,
            // default on -- it was accepted).
            if crate::loader_flags().verify_control_rules
                && !method_descriptor.ends_with(")V")
            {
                frame.set_fault(VerifyFault::BadCode {
                    header: "Method expects a return value".to_string(),
                });
                return Err(verify_err("Method expects a return value"));
            }
            if _method_name == "<init>"
                && current_class_name != "java/lang/Object"
                && frame.has_uninitialized_this()
            {
                // HotSpot's text (`flagThisUninit` at the `return`).
                frame.set_fault(VerifyFault::BadCode {
                    header: "Constructor must call super() or this() before return".to_string(),
                });
                return Err(verify_err(
                    "return: constructor returns while `this` is still uninitialized — \
                     no this()/super() constructor call was verified (JVMS §4.10.1.9)",
                ));
            }
            ok_no_fallthrough()
        }

        // =====================================================================
        // Field access — resolve type from constant pool
        // =====================================================================
        Instruction::Getstatic(index) => {
            let field_type = field_type_of(scratch, cp, *index)?;
            push_typed(frame, &field_type)?;
            ok_through()
        }

        Instruction::Putstatic(index) => {
            let field_type = field_type_of(scratch, cp, *index)?;
            pop_typed(frame, &field_type, hierarchy)?;
            ok_through()
        }

        Instruction::Getfield(index) => {
            let field_type = field_type_of(scratch, cp, *index)?;
            // HotSpot pops the objectref as the field's class.
            let objectref = pop_reference_or(frame, |i, actual| VerifyFault::BadStackType {
                index: i,
                actual,
                expected: member_owner_type(cp, *index),
            })?;
            if let Some((owner, _, _)) = member_ref_parts(cp, *index) {
                check_receiver_assignable(
                    frame,
                    scratch,
                    &objectref,
                    owner,
                    hierarchy,
                    "Bad type on operand stack",
                    "getfield",
                    None,
                )?;
            }
            check_protected_receiver_in(
                frame,
                scratch,
                cp,
                *index,
                &objectref,
                current_class_name,
                hierarchy,
                "getfield",
                false,
            )?;
            push_typed(frame, &field_type)?;
            ok_through()
        }

        Instruction::Putfield(index) => {
            let field_type = field_type_of(scratch, cp, *index)?;
            pop_typed(frame, &field_type, hierarchy)?; // value
            // HotSpot pops the objectref unchecked, then refuses it "in
            // putfield" against the field's class (constant pool origin).
            let objectref = pop_reference_or(frame, |i, actual| VerifyFault::BadTypeWith {
                header: "Bad type on operand stack in putfield".to_string(),
                index: i,
                actual,
                expected: member_owner_type(cp, *index),
                expected_cp: Some(*index),
            })?;
            if let Some((owner, _, _)) = member_ref_parts(cp, *index) {
                check_receiver_assignable(
                    frame,
                    scratch,
                    &objectref,
                    owner,
                    hierarchy,
                    "Bad type on operand stack in putfield",
                    "putfield",
                    Some(*index),
                )?;
            }
            // HotSpot 25 names the opcode: "... in putfield" (host w14b).
            check_protected_receiver_in(
                frame,
                scratch,
                cp,
                *index,
                &objectref,
                current_class_name,
                hierarchy,
                "putfield",
                false,
            )?;
            ok_through()
        }

        // =====================================================================
        // Method invocation
        // =====================================================================
        Instruction::Invokevirtual(index) => {
            let info = method_info(scratch, cp, *index, RefAccept::Method)?;
            check_internal_method_call(frame, &info, false)?;
            // T1.3.7 — reject `Reference.get0` (and siblings) on a
            // receiver that is not a `java/lang/ref/Reference` or
            // subclass. The JVMS requires the objectref to be
            // assignable to the method's declaring class; we enforce
            // it for the `Reference.*0` surface specifically because
            // those natives crash the VM if invoked on the wrong
            // shape, and HotSpot's verifier rejects the same pattern.
            // (`info.reference_special` is the method name when the owner is
            // `java/lang/ref/Reference` and the name is one of the three.)
            // Pop arguments in reverse order
            for param in info.sig.params.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            // Pop objectref
            let receiver = frame.pop()?;
            if !receiver.is_reference() {
                // HotSpot pops the receiver as the method's class
                // (`pop_stack(ref_class_type)`): "Bad type on operand stack"
                // naming that class (interpreter round i5 wave 101, lane L7).
                if let Some(owner) = &info.owner {
                    let expected = if owner.starts_with('[') {
                        VType::ArrayRef(Arc::clone(owner))
                    } else {
                        VType::ObjectRef(Arc::clone(owner))
                    };
                    let index = frame.stack.len();
                    frame.set_fault(VerifyFault::BadStackType {
                        index,
                        actual: HsType::V(receiver.clone()),
                        expected: HsType::V(expected),
                    });
                }
                return Err(verify_err(&format!(
                    "invokevirtual: receiver must be a reference, found {receiver:?}"
                )));
            }
            if let Some(owner) = &info.owner {
                check_receiver_assignable(
                    frame,
                    scratch,
                    &receiver,
                    owner,
                    hierarchy,
                    "Bad type on operand stack",
                    "invokevirtual",
                    None,
                )?;
            }
            check_protected_receiver_in(
                frame,
                scratch,
                cp,
                *index,
                &receiver,
                current_class_name,
                hierarchy,
                "invokevirtual",
                true,
            )?;
            if let Some(method_name) = &info.reference_special {
                {
                    // Receiver must be Reference-shaped (Reference or subclass),
                    // or Null (runtime NPE), or Top (dead code).
                    let is_ok = match &receiver {
                        super::vtype::VType::Null | super::vtype::VType::Top => true,
                        super::vtype::VType::ObjectRef(name) => {
                            // `name` is `&Arc<str>`; deref to `&str` to
                            // compare against string literals and pass into
                            // the hierarchy trait method.
                            let n: &str = name;
                            n == "java/lang/ref/Reference"
                                || n == "java/lang/ref/WeakReference"
                                || n == "java/lang/ref/SoftReference"
                                || n == "java/lang/ref/PhantomReference"
                                || hierarchy.is_subclass(n, "java/lang/ref/Reference")
                        }
                        _ => false,
                    };
                    if !is_ok {
                        return Err(verify_err(&format!(
                            "invokevirtual Reference.{method_name}: receiver {receiver:?} \
                             is not a java/lang/ref/Reference subclass"
                        )));
                    }
                }
            }
            // Push return type
            if let Some(ret_type) = &info.sig.ret {
                push_typed(frame, ret_type)?;
            }
            ok_through()
        }

        Instruction::Invokespecial(index) => {
            let info = method_info(scratch, cp, *index, RefAccept::MethodOrInterface)?;
            check_internal_method_call(frame, &info, true)?;
            // JVMS §4.10.1.9: the target class of a non-`<init>`
            // invokespecial (checked before the operands, as HotSpot does).
            if !info.is_init {
                check_invokespecial_target_in(
                    frame,
                    scratch,
                    cp,
                    *index,
                    current_class_name,
                    hierarchy,
                )?;
            }
            // Pop arguments in reverse order
            for param in info.sig.params.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            // Pop objectref (HotSpot: `reference_check()` for `<init>`, the
            // current class otherwise).
            let is_init = info.is_init;
            let receiver = pop_reference_or(frame, |i, actual| VerifyFault::BadStackType {
                index: i,
                actual,
                expected: if is_init {
                    HsType::Reference
                } else {
                    HsType::V(VType::ObjectRef(Arc::from(current_class_name)))
                },
            })?;
            // HotSpot: a non-`<init>` `invokespecial` pops its receiver as the
            // current class (`pop_stack(current_type())`).
            if !info.is_init {
                check_receiver_assignable(
                    frame,
                    scratch,
                    &receiver,
                    current_class_name,
                    hierarchy,
                    "Bad type on operand stack",
                    "invokespecial",
                    None,
                )?;
            }
            // JVMS 4.10.1.9: invokespecial on <init> initializes the receiver.
            // Replace all occurrences of the uninitialized type in both locals
            // and stack with the initialized ObjectRef(owner). Without this,
            // subsequent merges between the initialized path and the
            // uninitialized slot collapse to Top and break verification of
            // pre-Java-7 classes compiled without a StackMapTable (e.g.
            // picocli 4.x's `toCommandLine` helper).
            if info.is_init {
                // JVMS §4.10.1.9 (invokespecial <init>): the objectref MUST
                // be an *uninitialized* type — either `uninitializedThis` or
                // `uninitialized(offset)`. Calling `<init>` on an already
                // initialized reference (ObjectRef/ArrayRef), on `Null`, or on
                // a non-reference is a verification error: a sound verifier
                // must reject it (otherwise a class could re-run a constructor
                // on a fully-built object, or invoke `<init>` on a wrong type).
                //
                // After the call the spec replaces *all* occurrences of the
                // uninitialized type — in both locals and stack — with the
                // initialized class type:
                //   - For `uninitializedThis`, the initialized type is the
                //     current class (`this.<init>` chains to `super()`; `this`
                //     is still the current class, not the superclass owner).
                //     The declaring class of the invoked `<init>` must be the
                //     current class (this-delegating `this(...)`) or its direct
                //     superclass (`super(...)`).
                //   - For `uninitialized(offset)`, the initialized type is the
                //     method owner, which must match the class named by the
                //     `new` at `offset` (we resolve it from the method ref).
                // Interpreter round i5 wave 107, lane L7: HotSpot's
                // `verify_invoke_init` texts and faults ("Bad <init> method
                // call", "Call to wrong <init> method", "Bad operand type when
                // invoking <init>"); the last two rules were missing or worded
                // as our own.
                match &receiver {
                    VType::UninitializedThis => {
                        // The invoked constructor's owner must be either the
                        // current class or its directly linked superclass.
                        // Use the linked direct-superclass edge rather than a
                        // generic ancestry lookup: the latter can resolve a
                        // different loader-visible copy and would also accept
                        // an invalid grandparent constructor invocation.
                        if let Some(owner) = &info.owner {
                            let ok = **owner == *current_class_name
                                || hierarchy.is_direct_superclass(current_class_name, owner);
                            if !ok {
                                frame.set_fault(VerifyFault::BadTypeNoFrame {
                                    header: "Bad <init> method call".to_string(),
                                    actual: HsType::V(VType::ObjectRef(Arc::clone(owner))),
                                    actual_cp: None,
                                    expected: Some(HsType::V(VType::ObjectRef(Arc::from(
                                        current_class_name,
                                    )))),
                                    expected_cp: None,
                                });
                                return Err(verify_err(&format!(
                                    "Bad <init> method call: uninitializedThis receiver requires \
                                     the constructor owner to be the current class \
                                     ({current_class_name}) or its superclass, found {owner}"
                                )));
                            }
                        }
                        let replacement = VType::ObjectRef(Arc::from(current_class_name));
                        replace_vtype_in_frame(frame, &receiver, &replacement);
                    }
                    VType::Uninitialized(new_pc) => {
                        // The initialized type is the constructor's declaring
                        // class (which the bytecode's `new` site created).
                        let cls: Arc<str> = match &info.owner {
                            Some(o) => Arc::clone(o),
                            None => {
                                return Err(verify_err(
                                    "invokespecial <init>: cannot resolve constructor owner class",
                                ));
                            }
                        };
                        // HotSpot: "The method must be an <init> method of the
                        // indicated class" -- the class the `new` at `new_pc`
                        // names, not a superclass of it (that would leave the
                        // object's own constructor unrun).
                        let new_class_index = scratch
                            .new_sites_for(_method_name, method_descriptor)
                            .get(new_pc)
                            .copied();
                        if let Some(new_class_index) = new_class_index {
                            if let Some(new_class) = cp.get_class_name(new_class_index) {
                                if *new_class != *cls {
                                    let ref_class_index = match cp.get(*index) {
                                        Some(ConstantPoolEntry::MethodReference {
                                            class_index,
                                            ..
                                        }) => Some(*class_index),
                                        _ => None,
                                    };
                                    frame.set_fault(VerifyFault::BadTypeNoFrame {
                                        header: "Call to wrong <init> method".to_string(),
                                        actual: HsType::V(VType::ObjectRef(Arc::from(
                                            &*new_class,
                                        ))),
                                        actual_cp: Some(new_class_index),
                                        expected: Some(HsType::V(VType::ObjectRef(Arc::clone(
                                            &cls,
                                        )))),
                                        expected_cp: ref_class_index,
                                    });
                                    return Err(verify_err(&format!(
                                        "Call to wrong <init> method: the new at {new_pc} \
                                         creates {new_class}, the constructor is {cls}'s"
                                    )));
                                }
                            }
                        }
                        // HotSpot's protected rule for a constructor
                        // (`verify_invoke_init`; interpreter round i5 wave
                        // 108, lane L7): a `protected` `<init>` of a proper
                        // superclass in another run-time package may only
                        // initialize an object of the current class or a
                        // subclass, so `new Super; invokespecial Super.<init>`
                        // from a subclass in another package is refused.
                        // libverify (major < 50) has no such check
                        // (`major_version > LDC_CLASS_MAJOR_VERSION`).
                        if scratch.init_protected_rule && !scratch.skip_linkage_rules {
                            if let Some((_, _, init_descriptor)) = member_ref_parts(cp, *index) {
                                // Receiver-independent: cached per member
                                // reference, as `check_protected_receiver` does.
                                let applies = match scratch.protected_rule.get(index) {
                                    Some(&applies) => applies,
                                    None => {
                                        let applies = linkage_rule_protected_candidate(
                                            &cls,
                                            "<init>",
                                            init_descriptor,
                                            current_class_name,
                                            hierarchy,
                                            true,
                                        );
                                        scratch.protected_rule.insert(*index, applies);
                                        applies
                                    }
                                };
                                if applies && !hierarchy.is_subclass(&cls, current_class_name) {
                                    scratch.linkage_rule_violation = true;
                                    frame.set_fault(VerifyFault::BadTypeNoFrame {
                                        header: "Bad access to protected <init> method".to_string(),
                                        actual: HsType::V(VType::ObjectRef(Arc::clone(&cls))),
                                        actual_cp: new_class_index,
                                        expected: Some(HsType::V(VType::ObjectRef(Arc::from(
                                            current_class_name,
                                        )))),
                                        expected_cp: None,
                                    });
                                    return Err(verify_err(&format!(
                                        "Bad access to protected <init> method: {cls}.<init> is \
                                         protected in another run-time package and the object \
                                         is not a {current_class_name}"
                                    )));
                                }
                            }
                        }
                        let replacement = VType::ObjectRef(cls);
                        let to_replace = receiver.clone();
                        replace_vtype_in_frame(frame, &to_replace, &replacement);
                    }
                    // Already-initialized reference, Null, or any non-uninitialized
                    // type is not a legal receiver for `<init>` — reject.
                    other => {
                        frame.set_fault(VerifyFault::InvalidType {
                            header: "Bad operand type when invoking <init>".to_string(),
                            index: frame.stack.len(),
                            actual: HsType::V(other.clone()),
                        });
                        return Err(verify_err(&format!(
                            "Bad operand type when invoking <init>: receiver must be an \
                             uninitialized object (uninitializedThis or uninitialized(offset)), \
                             found {other:?}"
                        )));
                    }
                }
            }
            // Push return type
            if let Some(ret_type) = &info.sig.ret {
                push_typed(frame, ret_type)?;
            }
            ok_through()
        }

        Instruction::Invokestatic(index) => {
            let info = method_info(scratch, cp, *index, RefAccept::MethodOrInterface)?;
            check_internal_method_call(frame, &info, false)?;
            for param in info.sig.params.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            if let Some(ret_type) = &info.sig.ret {
                push_typed(frame, ret_type)?;
            }
            ok_through()
        }

        Instruction::Invokeinterface { index, count } => {
            let info = method_info(scratch, cp, *index, RefAccept::Interface)?;
            // HotSpot `verify_invoke_instructions` (interpreter round i6 wave
            // 116, lane L7): the `count` operand must be the argument slots
            // plus one (the receiver), "Inconsistent args count operand in
            // invokeinterface" (`bad_code`), checked before the internal-name
            // rule; libverify checks it after that rule ("Inconsistent
            // args_size for invokeinterface", `verify_old_text`). It was
            // accepted. Kill switch: `CRATONVM_VERIFY_CONTROL_RULES=0`.
            let count_ok = |info: &MethodRefInfo| {
                let slots: usize = info
                    .sig
                    .params
                    .iter()
                    .map(|p| if p.is_category2() { 2 } else { 1 })
                    .sum();
                usize::from(*count) == slots + 1
            };
            let count_rule = !scratch.skip_linkage_rules && crate::loader_flags().verify_control_rules;
            if count_rule && !scratch.old_texts && !count_ok(&info) {
                frame.set_fault(VerifyFault::BadCode {
                    header: "Inconsistent args count operand in invokeinterface".to_string(),
                });
                return Err(verify_err("Inconsistent args count operand in invokeinterface"));
            }
            check_internal_method_call(frame, &info, false)?;
            if count_rule && scratch.old_texts && !count_ok(&info) {
                frame.set_fault(VerifyFault::BadCode {
                    header: "Inconsistent args count operand in invokeinterface".to_string(),
                });
                return Err(verify_err("Inconsistent args count operand in invokeinterface"));
            }
            for param in info.sig.params.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            // objectref, popped as the interface (`ref_class_type`)
            pop_reference_or(frame, |i, actual| VerifyFault::BadStackType {
                index: i,
                actual,
                expected: member_owner_type(cp, *index),
            })?;
            if let Some(ret_type) = &info.sig.ret {
                push_typed(frame, ret_type)?;
            }
            ok_through()
        }

        Instruction::Invokedynamic(index) => {
            // Resolve the name and type from the InvokeDynamic CP entry
            let info = method_info(scratch, cp, *index, RefAccept::Dynamic)?;
            for param in info.sig.params.iter().rev() {
                pop_typed(frame, param, hierarchy)?;
            }
            if let Some(ret_type) = &info.sig.ret {
                push_typed(frame, ret_type)?;
            }
            ok_through()
        }

        // =====================================================================
        // Object creation and type checking
        // =====================================================================
        Instruction::New(index) => {
            // JVMS §4.9.1 / §6.5 new: the operand must be a `CONSTANT_Class`
            // entry naming a *class* type — not an array (`anewarray` /
            // `multianewarray` create those) and not an unrelated constant. The
            // previous arm ignored the index entirely, so `new #<any index>`
            // verified clean and the interpreter reached an entry of whatever
            // tag the class file happened to place there.
            // HotSpot's texts (interpreter round i5 wave 107, lane L7):
            // `verify_cp_class_type`'s "Illegal constant pool index N in class
            // C" / "Illegal type at constant pool entry N in class C"
            // (`bad_cp_index`), then "Illegal new instruction" for an array
            // class (`bad_type` of the cp type, no frame).
            let Some(class_name) = cp.get_class_name(*index) else {
                let external = current_class_name.replace('/', ".");
                let header = if *index == 0 || usize::from(*index) >= cp.len() {
                    format!("Illegal constant pool index {index} in class {external}")
                } else {
                    format!("Illegal type at constant pool entry {index} in class {external}")
                };
                let message = format!(
                    "{header} (new: constant pool index {index} is not a CONSTANT_Class entry \
                     with a resolvable name)"
                );
                frame.set_fault(VerifyFault::BadCpIndex {
                    header,
                    index: *index,
                });
                return Err(verify_err(&message));
            };
            if class_name.starts_with('[') {
                frame.set_fault(VerifyFault::BadTypeNoFrame {
                    header: "Illegal new instruction".to_string(),
                    actual: HsType::V(VType::ArrayRef(Arc::from(class_name))),
                    actual_cp: Some(*index),
                    expected: None,
                    expected_cp: None,
                });
                return Err(verify_err(&format!(
                    "Illegal new instruction (new: operand {class_name} is an array type; use \
                     anewarray / multianewarray, JVMS §6.5 new)"
                )));
            }
            if class_name.is_empty() {
                return Err(verify_err("new: operand names an empty class name"));
            }
            // Push Uninitialized(pc) — the object is uninitialized until <init> is called
            frame.push(VType::Uninitialized(pc as u16))?;
            // The class this site creates, for the `<init>` that initializes it
            // (wave 107, lane L7: "Call to wrong <init> method").
            scratch
                .new_sites_for(_method_name, method_descriptor)
                .insert(pc as u16, *index);
            ok_through()
        }

        Instruction::Newarray(_atype) => {
            // HotSpot `get_newarray_type` before the count pop: `bad_code`
            // "Illegal newarray instruction" (libverify: "Bad type passed to
            // newarray"). Interpreter round i5 wave 109, lane L7.
            if !(4..=11).contains(_atype) {
                frame.set_fault(VerifyFault::BadCode {
                    header: "Illegal newarray instruction".to_string(),
                });
                return Err(verify_err("Illegal newarray instruction"));
            }
            frame.pop_expect(&VType::Int, hierarchy)?; // count
                                                       // Result is an array of the given primitive type
                                                       // 4 boolean, 5 char, 6 float, 7 double, 8 byte, 9 short, 10 int,
                                                       // 11 long: `[Z [C [F [D [B [S [I [J`.
            let array_type = match _atype {
                4..=11 => with_well_known(|w| Arc::clone(&w.prim_arrays[(*_atype - 4) as usize])),
                _ => return Err(verify_err(&format!("newarray: invalid atype {_atype}"))),
            };
            frame.push(VType::ArrayRef(array_type))?;
            ok_through()
        }

        Instruction::Anewarray(index) => {
            frame.pop_expect(&VType::Int, hierarchy)?; // count
            if let Some(cached) = scratch.array_types.get(index) {
                frame.push(cached.clone())?;
                return ok_through();
            }
            let class_name = cp
                .get_class_name(*index)
                .ok_or_else(|| verify_err(&format!("anewarray: invalid class index {index}")))?;
            let desc = if class_name.starts_with('[') {
                // Array of arrays
                format!("[{class_name}")
            } else {
                format!("[L{class_name};")
            };
            let array_type = VType::ArrayRef(Arc::from(desc.as_str()));
            scratch.array_types.insert(*index, array_type.clone());
            frame.push(array_type)?;
            ok_through()
        }

        Instruction::Multianewarray { index, dimensions } => {
            // JVMS §4.10.1.9 multianewarray: `dimensions` must be >= 1, and
            // it must not exceed the number of array dimensions of the
            // referenced type (the leading `[` bracket count of the
            // descriptor). HotSpot raises a VerifyError for both.
            let class_name = cp.get_class_name_arc(*index).ok_or_else(|| {
                verify_err(&format!("multianewarray: invalid class index {index}"))
            })?;
            // The referenced type must be an array type with at least
            // `dimensions` leading brackets. HotSpot's order and texts
            // (interpreter round i5 wave 109, lane L7): a non-array class is
            // "Illegal constant pool index in multianewarray instruction"
            // (Reason "Invalid type: 'C' (constant pool N)"), then a zero or
            // too large dimension "Illegal dimension in multianewarray
            // instruction: D" (`bad_code`).
            let bracket_count = class_name.bytes().take_while(|&b| b == b'[').count();
            if bracket_count == 0 {
                let header = "Illegal constant pool index in multianewarray instruction";
                frame.set_fault(VerifyFault::BadTypeNoFrame {
                    header: header.to_string(),
                    actual: HsType::V(VType::ObjectRef(Arc::clone(&class_name))),
                    actual_cp: Some(*index),
                    expected: None,
                    expected_cp: None,
                });
                return Err(verify_err(header));
            }
            if *dimensions == 0 || bracket_count < *dimensions as usize {
                frame.set_fault(VerifyFault::BadCode {
                    header: format!("Illegal dimension in multianewarray instruction: {dimensions}"),
                });
            }
            if *dimensions == 0 {
                return Err(verify_err("multianewarray: dimensions must be >= 1"));
            }
            if bracket_count < *dimensions as usize {
                return Err(verify_err(&format!(
                    "multianewarray: dimensions {dimensions} exceeds array \
                     bracket count {bracket_count} of type {class_name}"
                )));
            }
            // Pop `dimensions` int values (counts), one per dimension.
            for _ in 0..*dimensions {
                frame.pop_expect(&VType::Int, hierarchy)?;
            }
            // The result is an array reference
            frame.push(VType::ArrayRef(class_name))?;
            ok_through()
        }

        Instruction::Arraylength => {
            pop_array_of(frame, hierarchy, "arraylength", ArrayKind::Any)?;
            frame.push(VType::Int)?;
            ok_through()
        }

        Instruction::Athrow => {
            // JVMS §4.10.1.6 / §6.5 athrow: the operand must be a
            // reference assignable to `java/lang/Throwable`. Throwing an
            // arbitrary non-Throwable reference is a type-safety
            // violation — the exception-dispatch machinery assumes a
            // Throwable shape.
            let throwable = VType::ObjectRef(with_well_known(|w| Arc::clone(&w.throwable)));
            let value = pop_reference_or(frame, |index, actual| VerifyFault::BadStackType {
                index,
                actual,
                expected: HsType::V(throwable.clone()),
            })?;
            // `Null` athrow is legal bytecode: it raises a
            // NullPointerException at runtime, which is the spec'd
            // outcome — accept it. `UninitializedThis` / `Uninitialized`
            // are rejected by the assignability check below (an
            // uninitialized object is not assignable to Throwable).
            let ok = matches!(value, VType::Null) || value.is_assignable_to(&throwable, hierarchy);
            if !ok {
                let index = frame.stack.len();
                frame.set_fault(VerifyFault::BadStackType {
                    index,
                    actual: HsType::V(value.clone()),
                    expected: HsType::V(throwable),
                });
                return Err(verify_err(&format!(
                    "athrow: operand {value:?} is not assignable to java/lang/Throwable"
                )));
            }
            ok_no_fallthrough()
        }

        Instruction::Checkcast(index) => {
            // HotSpot pops `java/lang/Object` (`object_type()`).
            pop_reference_or(frame, |i, actual| VerifyFault::BadStackType {
                index: i,
                actual,
                expected: HsType::V(object_vtype()),
            })?;
            let class_name = cp
                .get_class_name_arc(*index)
                .ok_or_else(|| verify_err(&format!("checkcast: invalid class index {index}")))?;
            if class_name.starts_with('[') {
                frame.push(VType::ArrayRef(class_name))?;
            } else {
                frame.push(VType::ObjectRef(class_name))?;
            }
            ok_through()
        }

        Instruction::Instanceof(index) => {
            pop_reference_or(frame, |i, actual| VerifyFault::BadStackType {
                index: i,
                actual,
                expected: HsType::V(object_vtype()),
            })?;
            // JVMS §6.5 instanceof: the operand must be a `CONSTANT_Class`
            // entry. Checked for the same reason as `new` / `checkcast` — the
            // index was previously ignored outright.
            if cp.get_class_name(*index).is_none() {
                return Err(verify_err(&format!(
                    "instanceof: constant pool index {index} is not a CONSTANT_Class \
                     entry with a resolvable name"
                )));
            }
            frame.push(VType::Int)?;
            ok_through()
        }

        // =====================================================================
        // Monitor — pop objectref
        // =====================================================================
        Instruction::Monitorenter | Instruction::Monitorexit => {
            pop_reference(frame, hierarchy)?;
            ok_through()
        }

        // =====================================================================
        // JSR / RET — legacy (pre-Java 7)
        // =====================================================================
        Instruction::Jsr(offset) => {
            // NEW-9: per JVMS §6.5.jsr, the value pushed is a
            // returnAddress whose value is the pc of the instruction
            // *immediately following* the jsr (pc+3). The prior code
            // pushed `ReturnAddress(target)` (the subroutine entry),
            // which is wrong: a later `ret N` would branch back to the
            // subroutine entry instead of to the call site.
            let target = branch_target(pc, *offset as i32);
            let next_pc = (pc + 3) as u16;
            frame.push(VType::ReturnAddress(next_pc))?;
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: BranchTargets::one(target),
            })
        }

        Instruction::JsrW(offset) => {
            // Same JVMS §6.5 contract. jsr_w is a 5-byte instruction
            // so the returnAddress is pc+5.
            let target = branch_target(pc, *offset);
            let next_pc = (pc + 5) as u16;
            frame.push(VType::ReturnAddress(next_pc))?;
            Ok(InsnVerifyResult {
                falls_through: false,
                branch_targets: BranchTargets::one(target),
            })
        }

        Instruction::Ret(index) => {
            // NEW-9: `ret N` jumps to the return address stored in local
            // slot N. The return address was pushed by an earlier `jsr`
            // and stored into the local by an `astore N` in the
            // subroutine prologue. The verifier reads the local's
            // current type-state — if it holds a `ReturnAddress(pc)`
            // the subroutine's exit flows to that pc. If it holds
            // anything else (or the slot is unset), the bytecode is
            // malformed: `ret` without a preceding `jsr` is invalid.
            //
            // This is an approximation of full JVMS §4.10.2.5 subroutine
            // verification — a rigorous implementation would also
            // unify the frame state across all jsr sites that target
            // the subroutine. For the worklist-based inference path we
            // use in pre-Java-7 classes, propagating the recorded
            // return pc as a branch target is sufficient to trace the
            // subroutine body back to the call site.
            let local = frame
                .local_load(*index as u16)
                .map_err(|_| verify_err(&format!("ret: local variable {index} out of range")))?;
            match local {
                VType::ReturnAddress(ret_pc) => Ok(InsnVerifyResult {
                    falls_through: false,
                    branch_targets: BranchTargets::one(*ret_pc),
                }),
                other => Err(verify_err(&format!(
                    "ret: local variable {index} must hold a returnAddress, \
                     got {other:?}"
                ))),
            }
        }

        // =====================================================================
        // Wide prefix — should have been decoded into wider variants
        // =====================================================================
        Instruction::Wide => {
            // Wide is a prefix handled during decoding; it should not appear standalone
            Err(verify_err("unexpected wide prefix in verification"))
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn ok_through() -> Result<InsnVerifyResult, LinkageError> {
    Ok(InsnVerifyResult {
        falls_through: true,
        branch_targets: BranchTargets::none(),
    })
}

fn ok_no_fallthrough() -> Result<InsnVerifyResult, LinkageError> {
    Ok(InsnVerifyResult {
        falls_through: false,
        branch_targets: BranchTargets::none(),
    })
}

/// Record HotSpot's "Bad local variable type" fault for a single-slot load of
/// local `index` that did not hold `expected` (interpreter round i5 wave 101,
/// lane L7; `verify_hotspot_text`).
#[cold]
fn set_local_fault(frame: &mut VerificationFrame, index: u16, expected: HsType) {
    let actual = VerifyFault::slot_type(&frame.locals, usize::from(index));
    frame.set_fault(VerifyFault::BadLocalType {
        index,
        actual,
        expected,
    });
}

/// Record the fault of a failed `long` / `double` load of the pair at `index`
/// (HotSpot's `get_local_2`): the base slot when it does not hold `base`, else
/// the upper slot. Nothing for an index past the frame (the bounds message
/// stands).
#[cold]
fn set_local_pair_fault(frame: &mut VerificationFrame, index: u16, base: &VType) {
    let lo = usize::from(index);
    if lo >= frame.locals.len() {
        // Past every slot the frame has: the bounds message stands.
        return;
    }
    let (slot, expected) = if frame.locals[lo] != *base || lo + 1 >= frame.locals.len() {
        (lo, HsType::V(base.clone()))
    } else if *base == VType::Double {
        (lo + 1, HsType::Double2nd)
    } else {
        (lo + 1, HsType::Long2nd)
    };
    let actual = VerifyFault::slot_type(&frame.locals, slot);
    frame.set_fault(VerifyFault::BadLocalPair {
        index,
        // `slot` is `index` or `index + 1`, below `frame.locals.len()`.
        slot: u16::try_from(slot).unwrap_or(index),
        actual,
        expected,
    });
}

fn verify_err(message: &str) -> LinkageError {
    LinkageError::VerifyError {
        class_name: String::new(),
        method_name: String::new(),
        message: message.to_string(),
    }
}

/// Prefix a `VerifyError` raised by a [`VerificationFrame`] helper with the
/// opcode that raised it, so the message names the rule *and* the instruction.
/// Non-`VerifyError` variants pass through untouched.
fn prefix_verify_err(opcode: &str, err: LinkageError) -> LinkageError {
    match err {
        LinkageError::VerifyError {
            class_name,
            method_name,
            message,
        } => LinkageError::VerifyError {
            class_name,
            method_name,
            message: format!("{opcode}: {message}"),
        },
        other => other,
    }
}

fn branch_target(pc: usize, offset: i32) -> u16 {
    // Use checked arithmetic to prevent overflow: truncating pc to i32
    // then to u16 could allow malformed bytecode to jump to arbitrary targets.
    let pc_i32: i32 = pc.try_into().unwrap_or(i32::MAX);
    let target = pc_i32.saturating_add(offset);
    if target < 0 || target > u16::MAX as i32 {
        // Out-of-range branch target — return 0 so the verifier rejects it
        // when it checks the target against the code length.
        0
    } else {
        target as u16
    }
}

/// Replace every occurrence of `old` in the frame's locals and stack with
/// `new`. Used to implement JVMS §4.10.1.9 rule for `invokespecial <init>`:
/// once the uninitialized receiver is initialized, every aliased copy of
/// the uninitialized type in the frame refers to the initialized object.
fn replace_vtype_in_frame(frame: &mut VerificationFrame, old: &VType, new: &VType) {
    // Only meaningful for uninitialized types; replacing a regular ObjectRef
    // is a no-op in normal flow but still safe.
    if !matches!(old, VType::Uninitialized(_) | VType::UninitializedThis) {
        return;
    }
    for slot in frame.locals.iter_mut() {
        if slot == old {
            *slot = new.clone();
        }
    }
    for slot in frame.stack.iter_mut() {
        if slot == old {
            *slot = new.clone();
        }
    }
}

/// Pop a reference type from the stack. A non-reference records HotSpot's
/// `reference_check()` fault ("Bad type on operand stack", expected
/// `reference type`; interpreter round i5 wave 102, lane L7).
fn pop_reference(
    frame: &mut VerificationFrame,
    _hierarchy: &dyn ClassHierarchy,
) -> Result<VType, LinkageError> {
    pop_reference_or(frame, |index, actual| VerifyFault::BadStackType {
        index,
        actual,
        expected: HsType::Reference,
    })
}

/// [`pop_reference`] where HotSpot pops with a named type: a non-reference
/// records `fault(slot index, type found there)`.
fn pop_reference_or(
    frame: &mut VerificationFrame,
    fault: impl FnOnce(usize, HsType) -> VerifyFault,
) -> Result<VType, LinkageError> {
    let n = frame.stack.len();
    let actual = n.checked_sub(1).map(|i| VerifyFault::slot_type(&frame.stack, i));
    let val = frame.pop()?;
    if !val.is_reference() {
        if let Some(actual) = actual {
            frame.set_fault(fault(n - 1, actual));
        }
        return Err(verify_err(&format!(
            "expected reference on stack, found {val:?}"
        )));
    }
    Ok(val)
}

/// HotSpot's name of a member reference's class (`ref_class_type`), for a
/// fault's expected type; `reference type` when the entry has none.
fn member_owner_type(cp: &ConstantPool, index: u16) -> HsType {
    match member_ref_parts(cp, index) {
        Some((owner, _, _)) if owner.starts_with('[') => HsType::V(VType::ArrayRef(Arc::from(owner))),
        Some((owner, _, _)) => HsType::V(VType::ObjectRef(Arc::from(owner))),
        None => HsType::Reference,
    }
}

/// `java/lang/Object` as a verification type.
fn object_vtype() -> VType {
    VType::ObjectRef(with_well_known(|w| Arc::clone(&w.object)))
}

/// What an array load / store / `arraylength` needs its arrayref to be
/// (HotSpot's `verify_method` cases; interpreter round i5 wave 102, lane L7).
#[derive(Clone, Copy)]
enum ArrayKind {
    /// Exactly this array type, named in the text (`'[I'`).
    Of(&'static str),
    /// `baload` / `bastore`: `[B` or `[Z`; the text names no expected type.
    ByteOrBool,
    /// `aaload` / `aastore`: an array of references (`reference type`).
    References,
    /// `arraylength`: any array; the text names no expected type.
    Any,
}

/// Pop the arrayref of `op` (null or an array of `kind`). A non-reference is
/// `reference_check()`'s "Bad type on operand stack"; a reference of the
/// wrong kind is "Bad type on operand stack in `op`". With
/// `CRATONVM_VERIFY_CONTROL_RULES=0` an `Of` / `ByteOrBool` operand only has
/// to be an array, as before (an array of references was always required of
/// `aastore`; `aaload` now requires one too, and an array in every mode: it
/// used to take ANY value).
fn pop_array_of(
    frame: &mut VerificationFrame,
    hierarchy: &dyn ClassHierarchy,
    op: &'static str,
    kind: ArrayKind,
) -> Result<VType, LinkageError> {
    let val = pop_reference(frame, hierarchy)?;
    let element_rules = crate::loader_flags().verify_control_rules;
    let ok = match (&val, kind) {
        (VType::Null, _) => true,
        (VType::ArrayRef(d), ArrayKind::Of(want)) => !element_rules || &**d == want,
        (VType::ArrayRef(d), ArrayKind::ByteOrBool) => {
            !element_rules || &**d == "[B" || &**d == "[Z"
        }
        (VType::ArrayRef(d), ArrayKind::References) => {
            matches!(d.as_bytes().get(1), Some(b'L') | Some(b'['))
        }
        (VType::ArrayRef(_), ArrayKind::Any) => true,
        _ => false,
    };
    if ok {
        return Ok(val);
    }
    let index = frame.stack.len();
    let header = format!("Bad type on operand stack in {op}");
    let actual = HsType::V(val.clone());
    frame.set_fault(match kind {
        ArrayKind::Of(want) => VerifyFault::BadTypeWith {
            header,
            index,
            actual,
            expected: HsType::V(VType::ArrayRef(Arc::from(want))),
            expected_cp: None,
        },
        ArrayKind::References => VerifyFault::BadTypeWith {
            header,
            index,
            actual,
            expected: HsType::Reference,
            expected_cp: None,
        },
        ArrayKind::ByteOrBool | ArrayKind::Any => VerifyFault::InvalidType {
            header,
            index,
            actual,
        },
    });
    Err(verify_err(&format!(
        "{op}: expected an array of the instruction's kind on stack, found {val:?}"
    )))
}

/// HotSpot's `verify_return_value` for a `*return` that popped `value`
/// (interpreter round i5 wave 102, lane L7; `CRATONVM_VERIFY_CONTROL_RULES`,
/// default on): a `void` method expects none ("Method does not expect a
/// return value"), any other method a value assignable to its declared
/// return type ("Bad return type"). `index` is the slot the value occupied.
fn check_return_value(
    frame: &mut VerificationFrame,
    value: &VType,
    index: usize,
    method_descriptor: &str,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    if !crate::loader_flags().verify_control_rules {
        return Ok(());
    }
    let declared = match parsed_sig(method_descriptor) {
        Some(sig) => sig.ret.clone(),
        None => return_type_from_descriptor(method_descriptor),
    };
    let (header, expected) = match declared {
        None => ("Method does not expect a return value", VType::Top),
        Some(declared) if value.is_assignable_to(&declared, hierarchy) => return Ok(()),
        Some(declared) => ("Bad return type", declared),
    };
    frame.set_fault(VerifyFault::BadReturn {
        header: header.to_string(),
        index,
        actual: HsType::V(value.clone()),
        expected: HsType::V(expected),
    });
    Err(verify_err(header))
}

/// Push a VType onto the stack, handling category-2 types (push + Top).
/// True if `popped` is the upper (`Top`) half of a category-2 value whose
/// `Long`/`Double` base is now exposed at the top of `frame`'s stack.
///
/// The verifier represents `long`/`double` as two slots — a `Long`/`Double`
/// base followed by a `Top`. Stack-manipulation instructions (`pop`, `dup`,
/// `swap`, …) operate on slots and must never split such a pair. After
/// `popped` has been removed from the stack, this checks whether `popped`
/// was that upper half.
fn is_cat2_upper_half(frame: &VerificationFrame, popped: &VType) -> bool {
    *popped == VType::Top && matches!(frame.stack.last(), Some(VType::Long) | Some(VType::Double))
}

/// Pop exactly `n` slots off the operand stack, returning them in
/// bottom-to-top order (`result[n-1]` was the top of stack).
fn pop_n_slots(
    frame: &mut VerificationFrame,
    n: usize,
    op: &str,
) -> Result<Vec<VType>, LinkageError> {
    if frame.stack.len() < n {
        return Err(verify_err(&format!(
            "{op}: stack underflow (need {n} slots, have {})",
            frame.stack.len()
        )));
    }
    let mut slots = Vec::with_capacity(n);
    for _ in 0..n {
        slots.push(frame.pop()?);
    }
    slots.reverse();
    Ok(slots)
}

/// Verify a slice of popped slots (bottom-to-top order) does not split a
/// category-2 value across its lower boundary. A category-2 base
/// (`Long`/`Double`) must always be immediately followed by its `Top`
/// upper half; the first slot must not be a dangling `Top`.
fn check_no_split(slots: &[VType], op: &str) -> Result<(), LinkageError> {
    // The first (lowest) slot may not be a `Top`: that would mean its
    // `Long`/`Double` base is still on the stack below — i.e. the pair was
    // split.
    if slots.first() == Some(&VType::Top) {
        return Err(verify_err(&format!(
            "{op}: operand splits a category-2 value"
        )));
    }
    // A `Long`/`Double` base as the last (highest) slot would mean its
    // `Top` upper half is above it and was not included — also a split.
    if matches!(slots.last(), Some(VType::Long) | Some(VType::Double)) {
        return Err(verify_err(&format!(
            "{op}: operand splits a category-2 value"
        )));
    }
    Ok(())
}

fn push_typed(frame: &mut VerificationFrame, vtype: &VType) -> Result<(), LinkageError> {
    frame.push(vtype.clone())?;
    if vtype.is_category2() {
        frame.push(VType::Top)?;
    }
    Ok(())
}

/// Pop a VType from the stack, handling category-2 types (pop Top + pop value).
fn pop_typed(
    frame: &mut VerificationFrame,
    vtype: &VType,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    if vtype.is_category2() {
        // HotSpot's `pop_stack_2`: the upper half first (interpreter round i5
        // wave 101, lane L7).
        return frame.pop_expect_cat2(vtype, hierarchy);
    }
    frame.pop_expect(vtype, hierarchy)?;
    Ok(())
}

/// Verify ldc: push Int, Float, String, or Class.
/// HotSpot's `verify_cp_index` / `verify_cp_type` refusal of an `ldc`,
/// `ldc_w` or `ldc2_w` operand (`ErrorContext::bad_cp_index`): "Illegal
/// constant pool index N in class C" for an index outside the pool, else
/// "Illegal type at constant pool entry N in class C" (a tag the opcode does
/// not load, or a `CONSTANT_Dynamic` of the other category, which
/// `verify_ldc` re-checks against the mask without `Dynamic`). Interpreter
/// round i5 wave 108, lane L7: the walk refused with its own words.
fn ldc_cp_refusal(
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    index: u16,
    current_class_name: &str,
    opcode: &str,
    detail: &str,
) -> LinkageError {
    let external = current_class_name.replace('/', ".");
    let header = if index == 0 || usize::from(index) >= cp.len() {
        format!("Illegal constant pool index {index} in class {external}")
    } else {
        format!("Illegal type at constant pool entry {index} in class {external}")
    };
    let message = format!("{header} ({opcode}: {detail})");
    frame.set_fault(VerifyFault::BadCpIndex { header, index });
    verify_err(&message)
}

fn verify_ldc(
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    index: u16,
    current_class_name: &str,
    class_refused: bool,
) -> Result<InsnVerifyResult, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::Integer(_)) => {
            frame.push(VType::Int)?;
        }
        // libverify below major 49 (`LDC_CLASS_MAJOR_VERSION`): a class
        // literal is not loadable (interpreter round i5 wave 109, lane L7).
        Some(ConstantPoolEntry::ClassReference { .. }) if class_refused => {
            return Err(ldc_cp_refusal(
                frame,
                cp,
                index,
                current_class_name,
                "ldc",
                "a CONSTANT_Class below class-file version 49",
            ));
        }
        Some(ConstantPoolEntry::Float(_)) => {
            frame.push(VType::Float)?;
        }
        Some(ConstantPoolEntry::StringReference { .. }) => {
            frame.push(VType::ObjectRef(with_well_known(|w| Arc::clone(&w.string))))?;
        }
        Some(ConstantPoolEntry::ClassReference { .. }) => {
            frame.push(VType::ObjectRef(with_well_known(|w| Arc::clone(&w.class))))?;
        }
        Some(ConstantPoolEntry::MethodHandle { .. }) => {
            frame.push(VType::ObjectRef(with_well_known(|w| {
                Arc::clone(&w.method_handle)
            })))?;
        }
        Some(ConstantPoolEntry::MethodType { .. }) => {
            frame.push(VType::ObjectRef(with_well_known(|w| {
                Arc::clone(&w.method_type)
            })))?;
        }
        Some(ConstantPoolEntry::Dynamic {
            name_and_type_index,
            ..
        }) => {
            // Dynamic constant — resolve type from NameAndType.
            if let Some((_, descriptor)) = cp.get_name_and_type(*name_and_type_index) {
                let vtype = VType::from_field_descriptor(descriptor);
                // SPEC-COMPLIANCE FIX (MED): `ldc` (and `ldc_w`) load a
                // *category-1* value only — JVMS §6.5 ldc requires the
                // referenced entry to NOT be `long`/`double`. A Dynamic
                // constant whose declared type is category-2 must be loaded
                // with `ldc2_w`; accepting it under `ldc` would push the
                // `Long`/`Double` base WITHOUT its paired `Top` upper half,
                // corrupting the verifier's category-2 slot model (every
                // subsequent stack offset would be off by one). Reject it.
                if vtype.is_category2() {
                    return Err(ldc_cp_refusal(
                        frame,
                        cp,
                        index,
                        current_class_name,
                        "ldc",
                        "Dynamic constant has a category-2 type (long/double); ldc loads \
                         category-1 only — use ldc2_w",
                    ));
                }
                frame.push(vtype)?;
            } else {
                return Err(verify_err("ldc: invalid Dynamic constant pool entry"));
            }
        }
        _ => {
            return Err(ldc_cp_refusal(
                frame,
                cp,
                index,
                current_class_name,
                "ldc",
                "not a loadable category-1 constant",
            ));
        }
    }
    ok_through()
}

/// Verify ldc2_w: push Long or Double (the category-2 forms).
fn verify_ldc2w(
    frame: &mut VerificationFrame,
    cp: &ConstantPool,
    index: u16,
    current_class_name: &str,
) -> Result<InsnVerifyResult, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::Long(_)) => {
            frame.push(VType::Long)?;
            frame.push(VType::Top)?;
        }
        Some(ConstantPoolEntry::Double(_)) => {
            frame.push(VType::Double)?;
            frame.push(VType::Top)?;
        }
        Some(ConstantPoolEntry::Dynamic {
            name_and_type_index,
            ..
        }) => {
            // SPEC-COMPLIANCE FIX (MED): `ldc2_w` is the category-2 loader and
            // since Java 11 may reference a Dynamic constant whose resolved
            // type is `long`/`double` (JVMS §6.5 ldc2_w). Push the base plus
            // its paired `Top` upper half so the category-2 slot model stays
            // consistent. A category-1 Dynamic constant under `ldc2_w` is
            // ill-formed (it belongs under `ldc`) — reject it.
            let descriptor = cp.get_name_and_type(*name_and_type_index).map(|(_, d)| d);
            match descriptor {
                Some(d) => {
                    let vtype = VType::from_field_descriptor(d);
                    if !vtype.is_category2() {
                        return Err(ldc_cp_refusal(
                            frame,
                            cp,
                            index,
                            current_class_name,
                            "ldc2_w",
                            "Dynamic constant has a category-1 type; ldc2_w loads category-2 \
                             (long/double) only — use ldc",
                        ));
                    }
                    frame.push(vtype)?;
                    frame.push(VType::Top)?;
                }
                None => {
                    return Err(verify_err("ldc2_w: invalid Dynamic constant pool entry"));
                }
            }
        }
        _ => {
            return Err(ldc_cp_refusal(
                frame,
                cp,
                index,
                current_class_name,
                "ldc2_w",
                "not a long, double or category-2 Dynamic constant",
            ));
        }
    }
    ok_through()
}

/// Resolve the field type descriptor from a FieldReference CP entry.
///
/// JVMS §4.4.2 / §4.4.6: a `Fieldref`'s `NameAndType` must carry a **field**
/// descriptor, and the referenced class must itself be a `CONSTANT_Class`.
/// Both are checked here.
///
/// SECURITY: rejecting a malformed descriptor is not pedantry.
/// [`VType::from_field_descriptor`] maps anything it does not recognise to
/// [`VType::Top`], and `Top` is the top of the assignability lattice — every
/// value is assignable to it. So a field whose descriptor is `"Q"` used to make
/// `putstatic` pop an arbitrary operand with no type check at all, and
/// `getstatic` push a `Top` that later merges could not constrain.
fn resolve_field_type(cp: &ConstantPool, index: u16) -> Result<VType, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) => {
            if cp.get_class_name(*class_index).is_none() {
                return Err(verify_err(&format!(
                    "field ref at index {index}: class_index {class_index} is not a \
                     CONSTANT_Class entry with a resolvable name"
                )));
            }
            let Some((name, descriptor)) = cp.get_name_and_type(*name_and_type_index) else {
                return Err(verify_err(&format!(
                    "field ref: invalid NameAndType at index {name_and_type_index}"
                )));
            };
            if name.is_empty() {
                return Err(verify_err(&format!(
                    "field ref at index {index}: empty field name"
                )));
            }
            match field_vtype(descriptor) {
                Some(t) => Ok(t),
                None => Err(verify_err(&format!(
                    "field ref at index {index}: {descriptor:?} is not a well-formed \
                     field descriptor (JVMS §4.3.2)"
                ))),
            }
        }
        _ => Err(verify_err(&format!(
            "expected FieldReference at constant pool index {index}"
        ))),
    }
}

/// Shared `NameAndType` cross-check for the `invoke*` family (JVMS §4.4.6).
///
/// The name must be non-empty and the descriptor must be a well-formed
/// **method** descriptor. A malformed descriptor would otherwise be parsed by
/// [`super::vtype::param_types_from_descriptor`] into a *short* parameter list,
/// so the verifier would pop fewer operands than the interpreter does at run
/// time — an operand-stack desynchronisation between the two.
fn checked_name_and_type(
    cp: &ConstantPool,
    name_and_type_index: u16,
    what: &str,
) -> Result<(String, String), LinkageError> {
    let Some((name, descriptor)) = cp.get_name_and_type(name_and_type_index) else {
        return Err(verify_err(&format!(
            "{what}: invalid NameAndType at index {name_and_type_index}"
        )));
    };
    if name.is_empty() {
        return Err(verify_err(&format!("{what}: empty method name")));
    }
    if !super::vtype::is_valid_method_descriptor(descriptor) {
        return Err(verify_err(&format!(
            "{what}: {descriptor:?} is not a well-formed method descriptor \
             (JVMS §4.3.3)"
        )));
    }
    Ok((name.to_string(), descriptor.to_string()))
}

/// Resolve method name and descriptor from a MethodReference CP entry.
fn resolve_method_name_and_type(
    cp: &ConstantPool,
    index: u16,
) -> Result<(String, String), LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::MethodReference {
            name_and_type_index,
            ..
        }) => checked_name_and_type(cp, *name_and_type_index, "method ref"),
        _ => Err(verify_err(&format!(
            "expected MethodReference at constant pool index {index}"
        ))),
    }
}

/// T1.3.7 — Resolve (owner_class, method_name, descriptor) from a
/// MethodReference so the verifier can cross-check the receiver type
/// at Invokevirtual against the method-owning class.
///
/// Used to reject calls like `Reference.get0` invoked on a receiver
/// that isn't `java/lang/ref/Reference`-shaped — a class of verifier
/// rejection HotSpot performs at link time.
fn resolve_method_owner_name_and_type(
    cp: &ConstantPool,
    index: u16,
) -> Option<(String, String, String)> {
    match cp.get(index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        } => {
            let owner = cp.get_class_name(*class_index)?.to_string();
            let (name, descriptor) = cp.get_name_and_type(*name_and_type_index)?;
            Some((owner, name.to_string(), descriptor.to_string()))
        }
        _ => None,
    }
}

/// Resolve method name and descriptor from either a MethodReference or
/// InterfaceMethodReference CP entry.  Per JVMS 4.9.1, `invokestatic` and
/// `invokespecial` may reference either kind since Java 8 (interface static
/// and default methods).
fn resolve_method_or_imethod_name_and_type(
    cp: &ConstantPool,
    index: u16,
) -> Result<(String, String), LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::MethodReference {
            name_and_type_index,
            ..
        })
        | Some(ConstantPoolEntry::InterfaceMethodReference {
            name_and_type_index,
            ..
        }) => checked_name_and_type(cp, *name_and_type_index, "method ref"),
        _ => Err(verify_err(&format!(
            "expected MethodReference or InterfaceMethodReference at constant pool index {index}"
        ))),
    }
}

/// Resolve method name and descriptor from an InterfaceMethodReference CP entry.
fn resolve_imethod_name_and_type(
    cp: &ConstantPool,
    index: u16,
) -> Result<(String, String), LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::InterfaceMethodReference {
            name_and_type_index,
            ..
        }) => checked_name_and_type(cp, *name_and_type_index, "interface method ref"),
        _ => Err(verify_err(&format!(
            "expected InterfaceMethodReference at constant pool index {index}"
        ))),
    }
}

/// Resolve the method type from an InvokeDynamic CP entry.
fn resolve_invokedynamic_type(cp: &ConstantPool, index: u16) -> Result<String, LinkageError> {
    match cp.get(index) {
        Some(ConstantPoolEntry::InvokeDynamic {
            name_and_type_index,
            ..
        }) => checked_name_and_type(cp, *name_and_type_index, "invokedynamic").map(|(_, d)| d),
        _ => Err(verify_err(&format!(
            "expected InvokeDynamic at constant pool index {index}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::verify_frame::VerificationFrame;
    use super::super::vtype::ClassHierarchy;
    use super::*;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    struct MockHierarchy;

    impl ClassHierarchy for MockHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            if child == parent || parent == "java/lang/Object" {
                return true;
            }
            // Model the JDK exception hierarchy so the `athrow` operand
            // check (operand must be assignable to java/lang/Throwable)
            // can be exercised by the tests.
            matches!(
                (child, parent),
                ("java/lang/Exception", "java/lang/Throwable")
                    | ("java/lang/RuntimeException", "java/lang/Throwable")
                    | ("java/lang/RuntimeException", "java/lang/Exception")
                    | ("java/lang/Error", "java/lang/Throwable")
            )
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    /// Interpreter round i5 wave 101, lane L7: HotSpot's receiver rule.
    #[test]
    fn receiver_rule_refuses_an_unassignable_loaded_receiver() {
        struct Unloaded;
        impl ClassHierarchy for Unloaded {
            fn is_subclass(&self, child: &str, parent: &str) -> bool {
                child == parent || parent == "java/lang/Object"
            }
            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }
            fn is_interface(&self, _name: &str) -> bool {
                false
            }
            fn is_resolvable(&self, name: &str) -> bool {
                name != "p/NotLoaded"
            }
        }
        let h = MockHierarchy;
        let mut scratch = VerifyScratch::for_class_version(52);
        scratch.receiver_rule = true;
        let mut frame = make_frame(1, 4);
        let class = VType::ObjectRef(Arc::from("java/lang/Class"));
        let err = check_receiver_assignable(
            &mut frame,
            &mut scratch,
            &class,
            "java/lang/String",
            &h,
            "Bad type on operand stack",
            "invokevirtual",
            None,
        )
        .expect_err("a Class is not a String");
        assert!(err.to_string().contains("invokevirtual"), "{err}");
        assert!(matches!(
            frame.take_fault(),
            Some(VerifyFault::BadTypeWith { index: 0, .. })
        ));
        // Cached: the same answer the second time.
        assert!(check_receiver_assignable(
            &mut frame,
            &mut scratch,
            &class,
            "java/lang/String",
            &h,
            "Bad type on operand stack",
            "invokevirtual",
            None,
        )
        .is_err());
        // Object targets, equal names, null and subclasses pass.
        for (recv, target) in [
            (class.clone(), "java/lang/Object"),
            (class.clone(), "java/lang/Class"),
            (VType::Null, "java/lang/String"),
            (VType::ObjectRef(Arc::from("java/lang/RuntimeException")), "java/lang/Exception"),
        ] {
            assert!(check_receiver_assignable(
                &mut frame, &mut scratch, &recv, target, &h, "h", "w", None
            )
            .is_ok());
        }
        // A pair the hierarchy cannot decide (a class not loaded yet) passes:
        // HotSpot would load it, this verifier cannot.
        let unloaded = VType::ObjectRef(Arc::from("p/NotLoaded"));
        assert!(check_receiver_assignable(
            &mut frame,
            &mut scratch,
            &unloaded,
            "java/lang/String",
            &Unloaded,
            "h",
            "w",
            None
        )
        .is_ok());
        // Switched off (`CRATONVM_VERIFY_RECEIVER_ASSIGNABLE=0`): no check.
        let mut off = VerifyScratch::default();
        assert!(check_receiver_assignable(
            &mut frame, &mut off, &class, "java/lang/String", &h, "h", "w", None
        )
        .is_ok());
    }

    fn simple_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,                        // 0
            ConstantPoolEntry::Integer(42),                      // 1
            ConstantPoolEntry::Float(3.125),                     // 2
            ConstantPoolEntry::Long(100.into()),                 // 3
            ConstantPoolEntry::Tombstone,                        // 4 (second slot of long)
            ConstantPoolEntry::Double(2.75.into()),              // 5
            ConstantPoolEntry::Tombstone,                        // 6 (second slot of double)
            ConstantPoolEntry::Utf8("java/lang/Object".into()),  // 7
            ConstantPoolEntry::ClassReference { name_index: 7 }, // 8
            ConstantPoolEntry::Utf8("value".into()),             // 9
            ConstantPoolEntry::Utf8("I".into()),                 // 10
            ConstantPoolEntry::NameAndType {
                name_index: 9,
                descriptor_index: 10,
            }, // 11
            ConstantPoolEntry::FieldReference {
                class_index: 8,
                name_and_type_index: 11,
            }, // 12
            ConstantPoolEntry::Utf8("toString".into()),          // 13
            ConstantPoolEntry::Utf8("()Ljava/lang/String;".into()), // 14
            ConstantPoolEntry::NameAndType {
                name_index: 13,
                descriptor_index: 14,
            }, // 15
            ConstantPoolEntry::MethodReference {
                class_index: 8,
                name_and_type_index: 15,
            }, // 16
            ConstantPoolEntry::StringReference { string_index: 7 }, // 17
        ])
    }

    fn make_frame(max_locals: u16, max_stack: u16) -> VerificationFrame {
        VerificationFrame::initial_frame("Test", "test", "()V", true, max_locals, max_stack)
    }

    #[test]
    fn verify_iconst_pushes_int() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let result = verify_instruction(
            &Instruction::Iconst0,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(result.falls_through);
        assert!(result.branch_targets.is_empty());
        assert_eq!(frame.stack_depth(), 1);
        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn verify_iadd_type_effect() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        frame.push(VType::Int).unwrap();
        frame.push(VType::Int).unwrap();

        verify_instruction(
            &Instruction::Iadd,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.stack_depth(), 1);
        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn verify_iadd_wrong_types_fails() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        frame.push(VType::Float).unwrap();
        frame.push(VType::Int).unwrap();

        // Second pop expects Int but finds Float
        let result = verify_instruction(
            &Instruction::Iadd,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(result.is_err());
    }

    #[test]
    fn verify_ldc_integer() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::Ldc(1),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn verify_ldc_string() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::Ldc(17),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(
            frame.pop().unwrap(),
            VType::ObjectRef(Arc::from("java/lang/String"))
        );
    }

    /// libverify loads a class literal with `ldc` only from major 49
    /// (interpreter round i5 wave 109, lane L7).
    #[test]
    fn ldc_class_is_refused_below_major_49() {
        let cp = simple_cp();
        let run = |major: u16| {
            let mut frame = make_frame(1, 4);
            let mut scratch = VerifyScratch::for_class_version(major);
            let result = verify_instruction_with(
                &Instruction::Ldc(8),
                0,
                &mut frame,
                &cp,
                "Test",
                "test",
                "()V",
                &MockHierarchy,
                &mut scratch,
            );
            (result.is_ok(), frame.take_fault())
        };
        let (ok, fault) = run(48);
        assert!(!ok);
        assert!(matches!(fault, Some(VerifyFault::BadCpIndex { index: 8, .. })));
        assert!(run(49).0);
        assert!(run(69).0);
        // A map-only walk keeps its maps.
        let mut frame = make_frame(1, 4);
        let mut scratch = VerifyScratch::for_class_version(48).without_linkage_rules();
        assert!(verify_instruction_with(
            &Instruction::Ldc(8),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &MockHierarchy,
            &mut scratch,
        )
        .is_ok());
    }

    /// HotSpot `verify_switch`: `lookupswitch` keys must be strictly
    /// increasing (interpreter round i5 wave 109, lane L7).
    #[test]
    fn unsorted_lookupswitch_is_refused() {
        let cp = simple_cp();
        let run = |pairs: Vec<(i32, i32)>| {
            let mut frame = make_frame(1, 4);
            frame.push(VType::Int).unwrap();
            let insn = Instruction::Lookupswitch(Arc::new(
                cratonvm_reader::instruction::LookupSwitch { default: 3, pairs },
            ));
            let result = verify_instruction_with(
                &insn,
                0,
                &mut frame,
                &cp,
                "Test",
                "test",
                "()V",
                &MockHierarchy,
                &mut VerifyScratch::for_class_version(69),
            );
            (result.is_ok(), frame.take_fault())
        };
        assert!(run(vec![(3, 3), (5, 3)]).0);
        for pairs in [vec![(5, 3), (3, 3)], vec![(3, 3), (3, 3)]] {
            let (ok, fault) = run(pairs);
            assert!(!ok);
            assert!(matches!(
                fault,
                Some(VerifyFault::BadCode { header }) if header == "Bad lookupswitch instruction"
            ));
        }
    }

    /// Constant pool with two Dynamic constants for the ldc/ldc2_w
    /// category tests: index #3 is a category-1 Dynamic (`I`), index #6 is
    /// a category-2 Dynamic (`J`).
    fn dynamic_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,        // 0
            ConstantPoolEntry::Utf8("d".into()), // 1
            ConstantPoolEntry::Utf8("I".into()), // 2 (category-1 descriptor)
            ConstantPoolEntry::NameAndType {
                name_index: 1,
                descriptor_index: 2,
            }, // 3 (NameAndType for cat-1)
            ConstantPoolEntry::Dynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 3,
            }, // 4 (category-1 Dynamic)
            ConstantPoolEntry::Utf8("J".into()), // 5 (category-2 descriptor)
            ConstantPoolEntry::NameAndType {
                name_index: 1,
                descriptor_index: 5,
            }, // 6 (NameAndType for cat-2)
            ConstantPoolEntry::Dynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 6,
            }, // 7 (category-2 Dynamic)
        ])
    }

    #[test]
    fn ldc_of_category2_dynamic_is_rejected() {
        // SPEC-COMPLIANCE FIX (MED): `ldc` of a Dynamic constant whose type
        // is category-2 (long/double) must be rejected — pushing it without
        // the paired `Top` would corrupt the verifier stack model.
        let cp = dynamic_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let res = verify_instruction(
            &Instruction::Ldc(7), // category-2 Dynamic
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(
            res.is_err(),
            "ldc of a category-2 Dynamic constant must be rejected, got {res:?}"
        );
        // The frame must be untouched on rejection (no half-pushed cat-2).
        assert_eq!(frame.stack_depth(), 0);
    }

    #[test]
    fn ldc_of_category1_dynamic_is_accepted() {
        // A category-1 Dynamic constant is the legal `ldc` case: pushes a
        // single slot, no Top.
        let cp = dynamic_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::Ldc(4), // category-1 Dynamic (`I`)
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .expect("ldc of a category-1 Dynamic must verify");
        assert_eq!(frame.stack_depth(), 1);
        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn ldc2w_of_category2_dynamic_pushes_top_slot() {
        // `ldc2_w` is the correct loader for a category-2 Dynamic constant:
        // it must push the Long/Double base plus its paired Top upper half.
        let cp = dynamic_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::Ldc2W(7), // category-2 Dynamic (`J`)
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .expect("ldc2_w of a category-2 Dynamic must verify");
        assert_eq!(frame.stack_depth(), 2);
        assert_eq!(frame.pop().unwrap(), VType::Top);
        assert_eq!(frame.pop().unwrap(), VType::Long);
    }

    #[test]
    fn ldc2w_of_category1_dynamic_is_rejected() {
        // A category-1 Dynamic under `ldc2_w` is ill-formed (belongs under
        // `ldc`) and must be rejected.
        let cp = dynamic_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let res = verify_instruction(
            &Instruction::Ldc2W(4), // category-1 Dynamic (`I`)
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(
            res.is_err(),
            "ldc2_w of a category-1 Dynamic constant must be rejected, got {res:?}"
        );
        assert_eq!(frame.stack_depth(), 0);
    }

    #[test]
    fn verify_getstatic() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        // getstatic index=12 → field "value" of type "I"
        verify_instruction(
            &Instruction::Getstatic(12),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.pop().unwrap(), VType::Int);
    }

    #[test]
    fn verify_goto_no_fallthrough() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let result = verify_instruction(
            &Instruction::Goto(10),
            5,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(!result.falls_through);
        assert_eq!(result.branch_targets, vec![15]);
    }

    #[test]
    fn verify_ifeq_branch() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Int).unwrap();

        let result = verify_instruction(
            &Instruction::Ifeq(20),
            5,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(result.falls_through);
        assert_eq!(result.branch_targets, vec![25]);
        assert_eq!(frame.stack_depth(), 0);
    }

    #[test]
    fn verify_return_no_fallthrough() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        let result = verify_instruction(
            &Instruction::Return,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(!result.falls_through);
    }

    #[test]
    fn verify_new_pushes_uninitialized() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        // new at PC=10, class index=8 (Object)
        verify_instruction(
            &Instruction::New(8),
            10,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.pop().unwrap(), VType::Uninitialized(10));
    }

    #[test]
    fn verify_aconst_null_pushes_null() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);

        verify_instruction(
            &Instruction::AconstNull,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.pop().unwrap(), VType::Null);
    }

    #[test]
    fn verify_dup() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Int).unwrap();

        verify_instruction(
            &Instruction::Dup,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert_eq!(frame.stack_depth(), 2);
    }

    #[test]
    fn verify_athrow_no_fallthrough() {
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/Exception")))
            .unwrap();

        let result = verify_instruction(
            &Instruction::Athrow,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();

        assert!(!result.falls_through);
    }

    // --- RVERIF.3 soundness regressions -----------------------------------

    #[test]
    fn athrow_rejects_non_throwable() {
        // A reference that is not assignable to java/lang/Throwable must
        // be rejected by athrow (JVMS §6.5 athrow).
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/String")))
            .unwrap();

        let result = verify_instruction(
            &Instruction::Athrow,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(result.is_err(), "athrow of a non-Throwable must fail");
    }

    #[test]
    fn athrow_accepts_null() {
        // `athrow null` is legal bytecode (raises NPE at runtime).
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();

        verify_instruction(
            &Instruction::Athrow,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .unwrap();
    }

    #[test]
    fn areturn_rejects_unassignable_return() {
        // Method declared to return java/lang/String must reject an
        // areturn of an unrelated reference type (JVMS §6.5 areturn).
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/Thread")))
            .unwrap();

        let result = verify_instruction(
            &Instruction::Areturn,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()Ljava/lang/String;",
            &h,
        );
        assert!(
            result.is_err(),
            "areturn of a type not assignable to the declared return type must fail"
        );
    }

    #[test]
    fn areturn_accepts_assignable_return() {
        // Returning the declared type itself succeeds.
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/String")))
            .unwrap();

        verify_instruction(
            &Instruction::Areturn,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()Ljava/lang/String;",
            &h,
        )
        .unwrap();
    }

    #[test]
    fn areturn_rejects_void_method() {
        // areturn must not appear in a method declared void.
        let cp = simple_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();

        let result = verify_instruction(
            &Instruction::Areturn,
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(result.is_err(), "areturn in a void method must fail");
    }

    /// Build a constant pool whose entry #6 is a `MethodReference` to
    /// `java/lang/Object.<init>()V`, for `invokespecial <init>` tests.
    fn init_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,                        // 0
            ConstantPoolEntry::Utf8("java/lang/Object".into()),  // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8("<init>".into()),            // 3
            ConstantPoolEntry::Utf8("()V".into()),               // 4
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
        ])
    }

    #[test]
    fn invokespecial_init_on_initialized_ref_rejected() {
        // JVMS §4.10.1.9: `<init>` may only be invoked on an *uninitialized*
        // receiver. An already-initialized ObjectRef must be rejected.
        let cp = init_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame
            .push(VType::ObjectRef(Arc::from("java/lang/Object")))
            .unwrap();

        let result = verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(
            result.is_err(),
            "invokespecial <init> on an initialized ObjectRef must fail verification"
        );
    }

    #[test]
    fn invokespecial_init_on_null_rejected() {
        // `Null` is a reference but not an uninitialized object — reject.
        let cp = init_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();

        let result = verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        );
        assert!(
            result.is_err(),
            "invokespecial <init> on Null must fail verification"
        );
    }

    #[test]
    fn invokespecial_init_on_uninitialized_initializes_slot() {
        // A `Uninitialized(offset)` receiver is legal; after the call every
        // aliased copy of that uninitialized type becomes the owner ObjectRef.
        let cp = init_cp();
        let h = MockHierarchy;
        let mut frame = make_frame(1, 4);
        // local 0 aliases the same uninitialized object (as if `dup`'d into it)
        frame.locals[0] = VType::Uninitialized(0);
        frame.push(VType::Uninitialized(0)).unwrap();

        verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Test",
            "test",
            "()V",
            &h,
        )
        .expect("invokespecial <init> on an uninitialized receiver must verify");

        // The receiver was popped; the aliased local must now be initialized.
        assert_eq!(
            frame.locals[0],
            VType::ObjectRef(Arc::from("java/lang/Object")),
            "uninitialized slot must be replaced with the initialized owner type"
        );
    }

    #[test]
    fn invokespecial_init_on_uninitialized_this_accepts_direct_superclass() {
        struct DirectSuperclassHierarchy;

        impl ClassHierarchy for DirectSuperclassHierarchy {
            fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
                false
            }

            fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
                child == "Example" && parent == "java/lang/Object"
            }

            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }

            fn is_interface(&self, _name: &str) -> bool {
                false
            }
        }

        let cp = init_cp();
        let h = DirectSuperclassHierarchy;
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        frame.push(VType::UninitializedThis).unwrap();

        verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
            &h,
        )
        .expect("uninitializedThis must be allowed to invoke its direct superclass constructor");

        assert_eq!(frame.locals[0], VType::ObjectRef(Arc::from("Example")));
    }

    #[test]
    fn invokespecial_init_on_uninitialized_this_rejects_indirect_superclass() {
        struct IndirectSuperclassHierarchy;

        impl ClassHierarchy for IndirectSuperclassHierarchy {
            fn is_subclass(&self, child: &str, parent: &str) -> bool {
                child == "Example" && parent == "java/lang/Object"
            }

            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }

            fn is_interface(&self, _name: &str) -> bool {
                false
            }
        }

        let cp = init_cp();
        let h = IndirectSuperclassHierarchy;
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        frame.push(VType::UninitializedThis).unwrap();

        let error = verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
            &h,
        )
        .expect_err("uninitializedThis must not invoke an indirect superclass constructor");

        assert!(error
            .to_string()
            .contains("uninitializedThis receiver requires the constructor owner"));
    }

    // =====================================================================
    // Constant-pool cross-checks (JVMS §4.9.1)
    // =====================================================================

    fn run(
        insn: &Instruction,
        frame: &mut VerificationFrame,
        cp: &ConstantPool,
        class: &str,
        method: &str,
        descriptor: &str,
    ) -> Result<InsnVerifyResult, LinkageError> {
        verify_instruction(
            insn,
            0,
            frame,
            cp,
            class,
            method,
            descriptor,
            &MockHierarchy,
        )
    }

    #[test]
    fn new_with_a_valid_class_entry_is_accepted() {
        let cp = init_cp(); // index 2 is a ClassReference → java/lang/Object
        let mut frame = make_frame(1, 4);
        run(&Instruction::New(2), &mut frame, &cp, "Test", "m", "()V")
            .expect("`new java/lang/Object` is well-formed");
        assert_eq!(frame.stack, vec![VType::Uninitialized(0)]);
    }

    #[test]
    fn new_with_a_non_class_cp_entry_rejected() {
        // Index 4 is a Utf8, not a CONSTANT_Class. The operand used to be
        // ignored entirely, so `new #4` verified clean.
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        let err = run(&Instruction::New(4), &mut frame, &cp, "Test", "m", "()V")
            .expect_err("`new` must reject a non-Class operand");
        assert!(err.to_string().contains("CONSTANT_Class"), "{err}");
    }

    #[test]
    fn new_with_an_out_of_range_cp_index_rejected() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        assert!(run(&Instruction::New(9999), &mut frame, &cp, "Test", "m", "()V").is_err());
    }

    #[test]
    fn new_of_an_array_type_rejected() {
        // JVMS §6.5 new: array types are created by anewarray / multianewarray.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("[Ljava/lang/Object;".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 },   // 2
        ]);
        let mut frame = make_frame(1, 4);
        let err = run(&Instruction::New(2), &mut frame, &cp, "Test", "m", "()V")
            .expect_err("`new` of an array type must be rejected");
        assert!(err.to_string().contains("array type"), "{err}");
    }

    #[test]
    fn instanceof_with_a_non_class_cp_entry_rejected() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();
        assert!(run(
            &Instruction::Instanceof(4),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V"
        )
        .is_err());
    }

    #[test]
    fn instanceof_with_a_valid_class_entry_accepted() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        frame.push(VType::Null).unwrap();
        run(
            &Instruction::Instanceof(2),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V",
        )
        .expect("`instanceof java/lang/Object` is well-formed");
        assert_eq!(frame.stack, vec![VType::Int]);
    }

    #[test]
    fn getstatic_with_a_malformed_field_descriptor_rejected() {
        // A descriptor `VType::from_field_descriptor` cannot parse becomes
        // `Top`, and everything is assignable to `Top` — so a malformed
        // descriptor used to disable the type check on this operand entirely.
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("Test".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8("f".into()),    // 3
            ConstantPoolEntry::Utf8("Ljava/lang/String".into()), // 4 — no ';'
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::FieldReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
        ]);
        let mut frame = make_frame(1, 4);
        let err = run(
            &Instruction::Getstatic(6),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V",
        )
        .expect_err("a malformed field descriptor must be a VerifyError");
        assert!(err.to_string().contains("field descriptor"), "{err}");
    }

    #[test]
    fn getstatic_with_a_well_formed_field_descriptor_accepted() {
        let cp = simple_cp(); // index 12 is Test.value : I
        let mut frame = make_frame(1, 4);
        run(
            &Instruction::Getstatic(12),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V",
        )
        .expect("a well-formed field ref must verify");
        assert_eq!(frame.stack, vec![VType::Int]);
    }

    #[test]
    fn invokestatic_with_a_malformed_method_descriptor_rejected() {
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("Test".into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8("m".into()),    // 3
            ConstantPoolEntry::Utf8("(Ljava/lang/String)V".into()), // 4 — no ';'
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
        ]);
        let mut frame = make_frame(1, 4);
        let err = run(
            &Instruction::Invokestatic(6),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V",
        )
        .expect_err("a malformed method descriptor must be a VerifyError");
        assert!(err.to_string().contains("method descriptor"), "{err}");
    }

    // =====================================================================
    // Category-2 local slot pairing (JVMS §4.10.1.6)
    // =====================================================================

    #[test]
    fn lstore_then_lload_round_trips() {
        let cp = simple_cp();
        let mut frame = make_frame(4, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V").unwrap();
        run(&Instruction::Lload(1), &mut frame, &cp, "Test", "m", "()V")
            .expect("an intact long pair must load back");
        assert_eq!(frame.stack, vec![VType::Long, VType::Top]);
    }

    #[test]
    fn splitting_a_long_pair_then_loading_it_rejected() {
        // `lstore_1; istore_2; lload_1` — the `istore_2` overwrote the long's
        // upper half, so `lload_1` must not verify.
        let cp = simple_cp();
        let mut frame = make_frame(4, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V").unwrap();
        frame.push(VType::Int).unwrap();
        run(&Instruction::Istore(2), &mut frame, &cp, "Test", "m", "()V").unwrap();
        let err = run(&Instruction::Lload(1), &mut frame, &cp, "Test", "m", "()V")
            .expect_err("a split category-2 pair must not load back as a long");
        assert!(err.to_string().contains("lload"), "{err}");
    }

    #[test]
    fn lstore_past_max_locals_rejected() {
        // `max_locals = 2` leaves slots 0 and 1; a long at slot 1 needs slot 2.
        let cp = simple_cp();
        let mut frame = make_frame(2, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        let err = run(&Instruction::Lstore(1), &mut frame, &cp, "Test", "m", "()V")
            .expect_err("a long store must fit entirely inside max_locals");
        assert!(err.to_string().contains("lstore"), "{err}");
    }

    #[test]
    fn wide_lstore_at_u16_max_does_not_panic() {
        // REGRESSION: `local_store(*index + 1, ..)` overflowed for a
        // `wide lstore 65535` — a debug panic on attacker-supplied bytecode.
        let cp = simple_cp();
        let mut frame = make_frame(4, 4);
        frame.push(VType::Long).unwrap();
        frame.push(VType::Top).unwrap();
        assert!(run(
            &Instruction::Lstore(u16::MAX),
            &mut frame,
            &cp,
            "Test",
            "m",
            "()V"
        )
        .is_err());
    }

    // =====================================================================
    // `<init>` must run before a constructor returns (JVMS §4.10.1.9)
    // =====================================================================

    #[test]
    fn constructor_return_before_super_init_rejected() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        let err = run(
            &Instruction::Return,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
        )
        .expect_err("a constructor may not return with `this` uninitialized");
        assert!(err.to_string().contains("uninitialized"), "{err}");
    }

    #[test]
    fn constructor_return_after_super_init_accepted() {
        struct DirectSuper;
        impl ClassHierarchy for DirectSuper {
            fn is_subclass(&self, child: &str, parent: &str) -> bool {
                child == parent || parent == "java/lang/Object"
            }
            fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
                child == "Example" && parent == "java/lang/Object"
            }
            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }
            fn is_interface(&self, _name: &str) -> bool {
                false
            }
        }

        let cp = init_cp();
        let h = DirectSuper;
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        frame.push(VType::UninitializedThis).unwrap();
        verify_instruction(
            &Instruction::Invokespecial(6),
            0,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
            &h,
        )
        .expect("super() call");
        verify_instruction(
            &Instruction::Return,
            4,
            &mut frame,
            &cp,
            "Example",
            "<init>",
            "()V",
            &h,
        )
        .expect("returning after super() is legal");
    }

    #[test]
    fn object_constructor_may_return_uninitialized() {
        // `java/lang/Object.<init>` has no superclass to chain to, so its
        // `uninitializedThis` is never replaced.
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        frame.locals[0] = VType::UninitializedThis;
        run(
            &Instruction::Return,
            &mut frame,
            &cp,
            "java/lang/Object",
            "<init>",
            "()V",
        )
        .expect("Object.<init> must be allowed to return");
    }

    #[test]
    fn ordinary_method_return_unaffected() {
        let cp = init_cp();
        let mut frame = make_frame(1, 4);
        run(&Instruction::Return, &mut frame, &cp, "Example", "m", "()V")
            .expect("an ordinary void return is unaffected");
    }

    // -----------------------------------------------------------------
    // JIT round 20 (lane verif): JVMS §4.10.1.8 protected receiver and
    // §4.10.1.9 invokespecial target rules.
    // -----------------------------------------------------------------

    /// `q/Cur extends p/Holder implements q/J`; `q/J extends q/I`; `p/Holder`
    /// declares protected `protF:I` and `prot()I` and public `pub()I`;
    /// `q/Sub extends q/Cur`; `p/Sib extends p/Holder`.
    struct RuleHierarchy;

    impl ClassHierarchy for RuleHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            child == parent
                || parent == "java/lang/Object"
                || matches!(
                    (child, parent),
                    ("q/Cur", "p/Holder")
                        | ("q/Sub", "q/Cur")
                        | ("q/Sub", "p/Holder")
                        | ("p/Sib", "p/Holder")
                )
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, name: &str) -> bool {
            name == "q/I" || name == "q/J"
        }
        fn invokespecial_target(
            &self,
            current: &str,
            ref_class: &str,
        ) -> Option<super::super::vtype::SpecialTarget> {
            use super::super::vtype::SpecialTarget;
            assert_eq!(current, "q/Cur");
            match ref_class {
                "p/Holder" | "q/J" => Some(SpecialTarget::SameOrDirect),
                "q/I" => Some(SpecialTarget::Assignable),
                "p/Other" => Some(SpecialTarget::NotAssignable),
                _ => None,
            }
        }
        fn protected_receiver_rule(
            &self,
            current: &str,
            ref_class: &str,
            name: &str,
            _descriptor: &str,
            _is_method: bool,
        ) -> bool {
            current == "q/Cur" && ref_class == "p/Holder" && (name == "protF" || name == "prot")
        }
    }

    /// 1 Utf8 p/Holder, 2 Class #1, 3 Utf8 protF, 4 Utf8 I, 5 NaT protF:I,
    /// 6 Fieldref p/Holder.protF:I, 7 Utf8 prot, 8 Utf8 ()I, 9 NaT prot()I,
    /// 10 Methodref p/Holder.prot()I, 11 Utf8 q/I, 12 Class #11,
    /// 13 Utf8 m, 14 NaT m()I, 15 InterfaceMethodref q/I.m()I,
    /// 16 Utf8 q/J, 17 Class #16, 18 InterfaceMethodref q/J.m()I,
    /// 19 Utf8 p/Other, 20 Class #19, 21 Methodref p/Other.m()I,
    /// 22 Utf8 pub, 23 NaT pub()I, 24 Methodref p/Holder.pub()I.
    fn rule_cp() -> ConstantPool {
        ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("p/Holder".into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8("protF".into()),
            ConstantPoolEntry::Utf8("I".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::FieldReference {
                class_index: 2,
                name_and_type_index: 5,
            },
            ConstantPoolEntry::Utf8("prot".into()),
            ConstantPoolEntry::Utf8("()I".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 7,
                descriptor_index: 8,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 9,
            },
            ConstantPoolEntry::Utf8("q/I".into()),
            ConstantPoolEntry::ClassReference { name_index: 11 },
            ConstantPoolEntry::Utf8("m".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 13,
                descriptor_index: 8,
            },
            ConstantPoolEntry::InterfaceMethodReference {
                class_index: 12,
                name_and_type_index: 14,
            },
            ConstantPoolEntry::Utf8("q/J".into()),
            ConstantPoolEntry::ClassReference { name_index: 16 },
            ConstantPoolEntry::InterfaceMethodReference {
                class_index: 17,
                name_and_type_index: 14,
            },
            ConstantPoolEntry::Utf8("p/Other".into()),
            ConstantPoolEntry::ClassReference { name_index: 19 },
            ConstantPoolEntry::MethodReference {
                class_index: 20,
                name_and_type_index: 14,
            },
            ConstantPoolEntry::Utf8("pub".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 22,
                descriptor_index: 8,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 23,
            },
        ])
    }

    fn rule_message(result: Result<InsnVerifyResult, LinkageError>) -> String {
        match result {
            Err(LinkageError::VerifyError { message, .. }) => message,
            Err(other) => panic!("expected a VerifyError, got {other:?}"),
            Ok(_) => panic!("expected a VerifyError, the instruction verified"),
        }
    }

    fn run_rule(
        insn: Instruction,
        receiver: VType,
        scratch: &mut VerifyScratch,
    ) -> Result<InsnVerifyResult, LinkageError> {
        let cp = rule_cp();
        let mut frame = make_frame(1, 4);
        frame.push(receiver).unwrap();
        verify_instruction_with(
            &insn,
            0,
            &mut frame,
            &cp,
            "q/Cur",
            "run",
            "()I",
            &RuleHierarchy,
            scratch,
        )
    }

    #[test]
    fn protected_getfield_through_a_sibling_receiver_is_refused() {
        let mut scratch = VerifyScratch::for_class_version(69);
        let result = run_rule(
            Instruction::Getfield(6),
            VType::ObjectRef(Arc::from("p/Sib")),
            &mut scratch,
        );
        assert_eq!(
            rule_message(result),
            "Bad access to protected data in getfield"
        );
        assert!(scratch.take_linkage_rule_violation());
        assert!(!scratch.take_linkage_rule_violation());
    }

    #[test]
    fn protected_getfield_through_own_null_or_subclass_receiver_verifies() {
        for receiver in [
            VType::ObjectRef(Arc::from("q/Cur")),
            VType::ObjectRef(Arc::from("q/Sub")),
            VType::Null,
        ] {
            let mut scratch = VerifyScratch::for_class_version(69);
            if let Err(e) = run_rule(Instruction::Getfield(6), receiver.clone(), &mut scratch) {
                panic!("{receiver:?} must verify: {e:?}");
            }
            assert!(!scratch.take_linkage_rule_violation());
        }
    }

    #[test]
    fn protected_invokevirtual_through_a_sibling_receiver_is_refused() {
        let mut scratch = VerifyScratch::for_class_version(69);
        let result = run_rule(
            Instruction::Invokevirtual(10),
            VType::ObjectRef(Arc::from("p/Sib")),
            &mut scratch,
        );
        assert_eq!(
            rule_message(result),
            "Bad access to protected data in invokevirtual"
        );
        // A public member of the same superclass is not protected access.
        let mut scratch = VerifyScratch::for_class_version(69);
        assert!(run_rule(
            Instruction::Invokevirtual(24),
            VType::ObjectRef(Arc::from("p/Sib")),
            &mut scratch,
        )
        .is_ok());
    }

    #[test]
    fn protected_rule_uses_the_old_verifier_text_and_is_off_for_map_walks() {
        let mut scratch = VerifyScratch::for_class_version(49);
        let result = run_rule(
            Instruction::Getfield(6),
            VType::ObjectRef(Arc::from("p/Sib")),
            &mut scratch,
        );
        assert_eq!(rule_message(result), "Bad access to protected data");
        let mut scratch = VerifyScratch::for_class_version(69).without_linkage_rules();
        assert!(run_rule(
            Instruction::Getfield(6),
            VType::ObjectRef(Arc::from("p/Sib")),
            &mut scratch,
        )
        .is_ok());
    }

    #[test]
    fn invokespecial_of_an_indirect_superinterface_is_refused() {
        let mut scratch = VerifyScratch::for_class_version(69);
        let result = run_rule(
            Instruction::Invokespecial(15),
            VType::ObjectRef(Arc::from("q/Cur")),
            &mut scratch,
        );
        assert_eq!(
            rule_message(result),
            "Bad invokespecial instruction: interface method to invoke is not in a direct \
             superinterface."
        );
        assert!(scratch.take_linkage_rule_violation());
    }

    #[test]
    fn invokespecial_of_an_unrelated_class_is_refused() {
        let mut scratch = VerifyScratch::for_class_version(69);
        let result = run_rule(
            Instruction::Invokespecial(21),
            VType::ObjectRef(Arc::from("q/Cur")),
            &mut scratch,
        );
        assert_eq!(
            rule_message(result),
            "Bad invokespecial instruction: current class isn't assignable to reference class."
        );
        let mut scratch = VerifyScratch::for_class_version(49);
        let result = run_rule(
            Instruction::Invokespecial(21),
            VType::ObjectRef(Arc::from("q/Cur")),
            &mut scratch,
        );
        assert_eq!(
            rule_message(result),
            "Illegal use of nonvirtual function call"
        );
    }

    #[test]
    fn invokespecial_of_a_direct_superinterface_or_superclass_verifies() {
        for index in [18u16, 24] {
            let mut scratch = VerifyScratch::for_class_version(69);
            if let Err(e) = run_rule(
                Instruction::Invokespecial(index),
                VType::ObjectRef(Arc::from("q/Cur")),
                &mut scratch,
            ) {
                panic!("cp #{index} must verify: {e:?}");
            }
        }
    }

    #[test]
    fn linkage_rule_candidates_are_decided_without_the_type_state() {
        let cp = rule_cp();
        let h = RuleHierarchy;
        let candidate = |insn: Instruction| linkage_rule_candidate(&insn, &cp, "q/Cur", &h, false);
        assert!(candidate(Instruction::Getfield(6)));
        assert!(candidate(Instruction::Putfield(6)));
        assert!(candidate(Instruction::Invokevirtual(10)));
        assert!(!candidate(Instruction::Invokevirtual(24)));
        assert!(candidate(Instruction::Invokespecial(15)));
        assert!(candidate(Instruction::Invokespecial(21)));
        assert!(!candidate(Instruction::Invokespecial(18)));
        // A hierarchy with no answer (mock / tooling) applies no rule.
        let mock = MockHierarchy;
        assert!(!linkage_rule_candidate(
            &Instruction::Invokespecial(21),
            &cp,
            "q/Cur",
            &mock,
            false
        ));
    }
}
