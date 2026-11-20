// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Verification types for the bytecode verifier (JVM spec 4.10.1).
//!
//! `VType` is the type domain used during bytecode verification — it is separate
//! from runtime `Value` because verification reasons about types, not values.
//! For example, `VType::Int` represents "any integer value" rather than a specific
//! integer.
//!
//! The `ClassHierarchy` trait decouples the verifier from `ClassStore`, enabling
//! mock testing.

use std::sync::Arc;

use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::stack_map::VerificationTypeInfo;

use cratonvm_types::error::LinkageError;

// ---------------------------------------------------------------------------
// ClassHierarchy trait — abstraction over class hierarchy queries
// ---------------------------------------------------------------------------

/// Provides class hierarchy information to the verifier.
///
/// Decouples the verifier from `ClassStore` so it can be tested with mock
/// hierarchies.
pub trait ClassHierarchy {
    /// Is `child` a subclass of (or implements) `parent`?
    /// Both names are internal form (e.g. `"java/lang/String"`).
    fn is_subclass(&self, child: &str, parent: &str) -> bool;

    /// Is `parent` the directly linked superclass of `child`?
    ///
    /// Constructor verification needs this stricter relationship for an
    /// `uninitializedThis` receiver. The default is deliberately conservative
    /// for lightweight test hierarchies; production hierarchy providers that
    /// retain linked class metadata should override it.
    fn is_direct_superclass(&self, _child: &str, _parent: &str) -> bool {
        false
    }

    /// Find the nearest common superclass of `a` and `b`.
    /// Returns `"java/lang/Object"` if no better common ancestor exists.
    fn common_superclass(&self, a: &str, b: &str) -> String;

    /// Is the named class an interface?
    fn is_interface(&self, name: &str) -> bool;

    /// Is the named class currently resolvable — i.e. already loaded so
    /// that hierarchy queries about it can return a definitive answer?
    ///
    /// SOUNDNESS NOTE (RVERIF.3): the verifier's assignability check no
    /// longer consults this hook to *accept* an otherwise-unprovable
    /// reference assignment. Treating "not yet loaded" as "assignable"
    /// was a verification escape hatch — a crafted class file could keep
    /// a type unloaded to bypass the type checker entirely. Per JVMS
    /// §4.10.1.2 the verifier must be conservative and reject what it
    /// cannot prove. This hook is retained for diagnostics / callers that
    /// want to distinguish "definitely not a subtype" from "unknown".
    ///
    /// The default returns `true` ("assume resolvable") so mock
    /// hierarchies used in unit tests keep their existing behaviour; the
    /// real `ClassStore`-backed implementation reports actual load state.
    fn is_resolvable(&self, _name: &str) -> bool {
        true
    }

    /// The VM's type-map store, which the verifier publishes the maps of the
    /// class it verifies into (`type_maps::store_for`). `ClassId` is a per-VM
    /// index, so maps keyed by it must not share one process-wide table; see
    /// `type_maps::TypeMapStore`.
    ///
    /// The default `None` (mock and tooling hierarchies) publishes into
    /// `type_maps::process_store`, which no execution decision reads.
    fn type_map_store(&self) -> Option<&crate::type_maps::TypeMapStore> {
        None
    }

    /// Does this hierarchy record the class loads HotSpot's verifier would
    /// make ([`Self::note_verifier_load`])? Asked once per reference
    /// assignability check, before any name is classified; the default (mock
    /// and tooling hierarchies, redefinition) records nothing.
    fn records_verifier_loads(&self) -> bool {
        false
    }

    /// HotSpot's split verifier would load `name` through the verified
    /// class's loader here (`VerificationType::resolve_and_check_assignability`).
    /// CratonVM verifies at define time and never runs a loader from the
    /// verifier, so the define-time hierarchy only records the name; the
    /// VM's link step loads it (`i47-verifier-assignability-checks-initiate-no-loads`).
    fn note_verifier_load(&self, _name: &str) {}

    /// HotSpot's verifier would load `source` here only if `target` (which
    /// it has just loaded, [`Self::note_verifier_load`]) turns out to be a
    /// class: an interface target is answered "assignable" without loading
    /// the source (`VerificationType::is_reference_assignable_from`). The
    /// define-time hierarchy cannot tell yet when `target` is not loaded, so
    /// it records the pair and the VM's link step decides once `target` is
    /// loaded ([`VerifierLinkLoad::SourceUnlessInterface`]). Loading the
    /// source unconditionally fails the link of a class that merely stores
    /// an optional-dependency type into an interface-typed slot (Spring's
    /// `BeanDefinitionLoader` storing a `GroovyBeanDefinitionReader` into a
    /// `BeanDefinitionReader` field: `NoClassDefFoundError:
    /// groovy/lang/GroovyObject` where HotSpot links). The default (mock
    /// hierarchies) records the source as an unconditional load.
    fn note_verifier_load_unless_interface(&self, _target: &str, source: &str) {
        self.note_verifier_load(source);
    }

    /// HotSpot's `invokespecial` target rule (`ClassVerifier::
    /// verify_invoke_instructions`, JVMS §4.10.1.9) for a non-`<init>`
    /// reference to `ref_class` made by the class under verification
    /// `current` (`ref_class != current`). `None` = no answer, no check: the
    /// default (mock and tooling hierarchies), a class HotSpot does not
    /// verify (boot loader, hidden, VM-generated, reflection accessors), or a
    /// `ref_class` that is not loaded (HotSpot would load it; this verifier
    /// does not).
    fn invokespecial_target(&self, _current: &str, _ref_class: &str) -> Option<SpecialTarget> {
        None
    }

    /// HotSpot's `verify_exception_handler_table` catch-type check, asked
    /// before a method's code is walked (interpreter round i5 wave 103, lane
    /// L7): is the ALREADY LOADED class `name` a `java/lang/Throwable`?
    /// `None` = no answer, no check: the default (mock and tooling
    /// hierarchies), or a catch type not loaded yet (HotSpot would load it
    /// here; the VM's link step loads it after the walk,
    /// `load_catch_types_at_link`, and refuses it there).
    fn catch_type_is_throwable(&self, _name: &str) -> Option<bool> {
        None
    }

    /// HotSpot's `name_in_supers` + `is_protected_access` (JVMS §4.10.1.8)
    /// for a `getfield` / `putfield` (`is_method == false`) or
    /// `invokevirtual` (`is_method == true`) of `name:descriptor` through
    /// `ref_class`, made by the class under verification `current`: `true`
    /// when `ref_class` names a proper superclass of `current` and the member
    /// it resolves to (looked up from that superclass upward) is `protected`
    /// and declared in another run-time package, so the objectref must be
    /// assignable to `current`. The default (mock and tooling hierarchies,
    /// classes HotSpot does not verify) is `false`: no check.
    fn protected_receiver_rule(
        &self,
        _current: &str,
        _ref_class: &str,
        _name: &str,
        _descriptor: &str,
        _is_method: bool,
    ) -> bool {
        false
    }

    /// The common superclass of two object types as a merged type's name
    /// ([`VType::merge`]). The default is [`Self::common_superclass`];
    /// `bytecode_verifier::MergeFallbackWatch` (the libverify-rules inference
    /// walk, interpreter round i5 wave 112, lane L7) hands out a NAMED
    /// instance for a fallback over a class not loaded, so the merged value
    /// carries that fact ([`Self::is_fallback_merge`]).
    fn merged_class_name(&self, a: &str, b: &str) -> Arc<str> {
        Arc::from(self.common_superclass(a, b).as_str())
    }

    /// A hierarchy's own merge of two reference types, ahead of
    /// [`VType::merge`]'s arms (`None` = use them; the default). The
    /// libverify-rules walk answers arrays as `check_code.c`
    /// `merge_fullinfo_types` does (wave 112).
    fn merge_references(&self, _a: &VType, _b: &VType) -> Option<VType> {
        None
    }

    /// Is `name` (by identity: the `Arc` a merge produced) a merged type that
    /// fell back to `java/lang/Object` because a class was not loaded? The
    /// default is `false` (wave 112).
    fn is_fallback_merge(&self, _name: &Arc<str>) -> bool {
        false
    }
}

/// How the class referenced by a non-`<init>` `invokespecial` relates to the
/// class under verification ([`ClassHierarchy::invokespecial_target`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialTarget {
    /// The current class's direct superclass or one of its DIRECT
    /// superinterfaces (`is_same_or_direct_interface`): no rule applies.
    SameOrDirect,
    /// Assignable from the current class in HotSpot's verifier sense: a
    /// farther superclass, or an interface (the split verifier treats every
    /// interface type as assignable from any class).
    Assignable,
    /// A class the current class is not a subclass of.
    NotAssignable,
}

/// One class load HotSpot's split verifier makes through a class's defining
/// loader while linking it, recorded by CratonVM's define-time verification
/// and made by the VM's link step (`ClassManager::verifier_link_loads`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifierLinkLoad {
    /// Load this name.
    Name(Arc<str>),
    /// Load `source` unless `target` -- loaded by an earlier entry of the
    /// same row -- is an interface. Recorded when `target` was not loaded at
    /// define time ([`ClassHierarchy::note_verifier_load_unless_interface`]).
    SourceUnlessInterface {
        /// The assignability check's target type, loaded first.
        target: Arc<str>,
        /// The source type, loaded only for a class target.
        source: Arc<str>,
    },
}

impl VerifierLinkLoad {
    /// The name this entry may load (the source of a conditional entry).
    pub fn loaded_name(&self) -> &Arc<str> {
        match self {
            VerifierLinkLoad::Name(name) => name,
            VerifierLinkLoad::SourceUnlessInterface { source, .. } => source,
        }
    }
}

/// Record the loads HotSpot's `VerificationType::is_reference_assignable_from`
/// makes for `source` assigned to `target` (two types already known to
/// differ): the target unless it is `java/lang/Object`, then the source when
/// the target is a class (an interface target is answered without it), and
/// for two reference arrays the same rule for their components.
pub(crate) fn note_hotspot_assignability_loads(
    source: &VType,
    target: &VType,
    hierarchy: &dyn ClassHierarchy,
) {
    match (source, target) {
        (VType::ObjectRef(from), VType::ObjectRef(to)) => {
            if &**to == "java/lang/Object" {
                return;
            }
            hierarchy.note_verifier_load(to);
            if hierarchy.is_interface(to) || is_known_jdk_interface(to) {
                // An interface target: HotSpot answers without the source.
            } else if hierarchy.is_resolvable(to) {
                // A loaded class target: HotSpot loads the source.
                hierarchy.note_verifier_load(from);
            } else {
                // Not loaded yet: whether it is an interface is known only
                // once the link step has loaded it.
                hierarchy.note_verifier_load_unless_interface(to, from);
            }
        }
        (VType::ArrayRef(_), VType::ObjectRef(to)) => {
            if &**to != "java/lang/Object" {
                hierarchy.note_verifier_load(to);
            }
        }
        (VType::ArrayRef(from), VType::ArrayRef(to)) => {
            let (Some(from_elem), Some(to_elem)) = (from.strip_prefix('['), to.strip_prefix('['))
            else {
                return;
            };
            if from_elem == to_elem {
                return;
            }
            let component = |elem: &str| match elem.as_bytes().first() {
                Some(b'[') => Some(VType::ArrayRef(Arc::from(elem))),
                Some(b'L') => strip_object_descriptor(elem).map(|n| VType::ObjectRef(Arc::from(n))),
                _ => None,
            };
            if let (Some(from), Some(to)) = (component(from_elem), component(to_elem)) {
                note_hotspot_assignability_loads(&from, &to, hierarchy);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// VType — verification type domain
// ---------------------------------------------------------------------------

/// A verification type — the type of a single local variable slot or stack entry.
///
/// Unlike runtime `Value`, `VType` represents type categories for verification:
/// - `Top` = undefined/unusable (e.g. second slot of long/double)
/// - `Int` = any int-category-1 type (int, short, byte, char, boolean)
/// - `Float`, `Long`, `Double` = their respective types
/// - `Null` = the null reference
/// - `ObjectRef("java/lang/String")` = a reference to a class instance
/// - `ArrayRef("[I")` = a reference to an array
/// - `UninitializedThis` = `this` before `<init>` call in constructor
/// - `Uninitialized(offset)` = object created by `new` at bytecode offset
/// - `ReturnAddress(offset)` = target of `jsr` (legacy, pre-Java-7)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VType {
    /// Undefined / unusable slot (second half of Long/Double).
    Top,
    /// Integer (includes boolean, byte, char, short).
    Int,
    /// Float.
    Float,
    /// Long (category-2, occupies two slots).
    Long,
    /// Double (category-2, occupies two slots).
    Double,
    /// The null reference (assignable to any reference type).
    Null,
    /// A reference to an instance of the named class.
    /// Name is in internal form: `"java/lang/Object"`.
    ///
    /// Stored as `Arc<str>` so frame clones (which happen on every
    /// branch target, exception handler entry, and worklist iteration)
    /// only bump a refcount instead of heap-allocating every class name.
    /// The names come from the constant pool (already `Arc<str>`), so
    /// constructing one is a `Arc::clone`, not an allocation.
    ObjectRef(Arc<str>),
    /// A reference to an array type.
    /// Descriptor is in field descriptor form: `"[I"`, `"[Ljava/lang/String;"`.
    ///
    /// Same `Arc<str>` rationale as [`VType::ObjectRef`].
    ArrayRef(Arc<str>),
    /// The uninitialized `this` reference in a constructor (before `<init>` call).
    UninitializedThis,
    /// A reference to an uninitialized object created by `new` at the given offset.
    Uninitialized(u16),
    /// Return address for `jsr/ret` (legacy, Java < 7).
    ReturnAddress(u16),
}

impl VType {
    /// Is this a category-2 type (Long or Double)?
    pub fn is_category2(&self) -> bool {
        matches!(self, VType::Long | VType::Double)
    }

    /// Is this a reference type (Object, Array, Null, UninitializedThis, Uninitialized)?
    pub fn is_reference(&self) -> bool {
        matches!(
            self,
            VType::ObjectRef(_)
                | VType::ArrayRef(_)
                | VType::Null
                | VType::UninitializedThis
                | VType::Uninitialized(_)
        )
    }

    /// Convert a `VerificationTypeInfo` from a StackMapTable frame into a `VType`.
    ///
    /// For `Object` types, resolves the constant pool index to a class name.
    pub fn from_verification_type_info(
        info: &VerificationTypeInfo,
        cp: &ConstantPool,
    ) -> Result<Self, LinkageError> {
        match info {
            VerificationTypeInfo::Top => Ok(VType::Top),
            VerificationTypeInfo::Integer => Ok(VType::Int),
            VerificationTypeInfo::Float => Ok(VType::Float),
            VerificationTypeInfo::Double => Ok(VType::Double),
            VerificationTypeInfo::Long => Ok(VType::Long),
            VerificationTypeInfo::Null => Ok(VType::Null),
            VerificationTypeInfo::UninitializedThis => Ok(VType::UninitializedThis),
            VerificationTypeInfo::Object { cpool_index } => {
                let class_name = cp.get_class_name_arc(*cpool_index).ok_or_else(|| {
                    LinkageError::VerifyError {
                        class_name: String::new(),
                        method_name: String::new(),
                        message: format!(
                            "invalid class reference at constant pool index {cpool_index}"
                        ),
                    }
                })?;
                // Determine if it's an array type or object type. The
                // `Arc<str>` is moved into the chosen variant — no
                // allocation, no `.to_string()`.
                if class_name.starts_with('[') {
                    Ok(VType::ArrayRef(class_name))
                } else {
                    Ok(VType::ObjectRef(class_name))
                }
            }
            VerificationTypeInfo::Uninitialized { offset } => Ok(VType::Uninitialized(*offset)),
        }
    }

    /// Convert a field descriptor to a `VType`.
    ///
    /// Examples:
    /// - `"I"` → `Int`
    /// - `"J"` → `Long`
    /// - `"D"` → `Double`
    /// - `"F"` → `Float`
    /// - `"B"` / `"C"` / `"S"` / `"Z"` → `Int`
    /// - `"Ljava/lang/String;"` → `ObjectRef("java/lang/String")`
    /// - `"[I"` → `ArrayRef("[I")`
    /// - `"[[Ljava/lang/Object;"` → `ArrayRef("[[Ljava/lang/Object;")`
    pub fn from_field_descriptor(descriptor: &str) -> Self {
        match descriptor.as_bytes().first() {
            Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => VType::Int,
            Some(b'J') => VType::Long,
            Some(b'F') => VType::Float,
            Some(b'D') => VType::Double,
            Some(b'L') => {
                // Strip 'L' prefix and ';' suffix to get the class name.
                // A malformed L-type with no terminating ';' (e.g. the
                // bare string "L", or "Ljava/lang/String" with no ';')
                // must NOT panic on the slice — fall back to Top so
                // verification fails cleanly later, matching the
                // `_ => VType::Top` arm below.
                match descriptor
                    .strip_prefix('L')
                    .and_then(|s| s.strip_suffix(';'))
                {
                    Some(class_name) => VType::ObjectRef(Arc::from(class_name)),
                    None => VType::Top, // malformed object descriptor
                }
            }
            Some(b'[') => VType::ArrayRef(Arc::from(descriptor)),
            _ => VType::Top, // invalid descriptor → Top (verification will fail later)
        }
    }

    /// Check if this type is assignable to `target` in the verification type lattice.
    ///
    /// Per JVM spec 4.10.1.2:
    /// - `Top` is assignable to `Top`
    /// - `Null` is assignable to any reference type
    /// - Subclass is assignable to superclass
    /// - Any type is assignable to itself
    /// - Array types: `[Child` assignable to `[Parent` (covariant for references)
    /// - All reference types assignable to `Object`
    pub fn is_assignable_to(&self, target: &VType, hierarchy: &dyn ClassHierarchy) -> bool {
        // Same type is always assignable
        if self == target {
            return true;
        }
        if hierarchy.records_verifier_loads() {
            note_hotspot_assignability_loads(self, target, hierarchy);
        }

        match (self, target) {
            // Per JVMS §4.10.1.2, Top is the top of the verification type
            // lattice — every verification type is assignable to Top.
            // StackMapTable frames use Top to mark slots whose runtime value
            // is unused past this merge point; a narrower type on one incoming
            // path must therefore widen to Top when the declared frame says so.
            (_, VType::Top) => true,

            // Top is only assignable to Top (handled by the case above).
            (VType::Top, _) => false,

            // Null is assignable to any reference type
            (VType::Null, VType::ObjectRef(_)) => true,
            (VType::Null, VType::ArrayRef(_)) => true,
            (VType::Null, VType::Null) => true, // handled by equality but explicit

            // Object subtyping.
            //
            // Per JVMS §4.10.1.2: "The verifier does not distinguish between
            // interface types and java.lang.Object in its tracking of
            // reference types." When the target is an interface, any
            // reference type is assignable to it at verify time — runtime
            // checkcast / invokeinterface enforce the actual type.
            //
            // RVERIF.2: also consult `is_known_jdk_interface` so that the
            // relaxation fires even when `parent` is loaded as a synthetic
            // stub that lacks the INTERFACE access flag. Without this,
            // assignments like `List` → `Collection` are rejected when both
            // are stubbed (the stub heuristic flags them as PUBLIC|SUPER,
            // not INTERFACE) and `is_subclass` returns false because the
            // stub's `interfaces` array is empty.
            (VType::ObjectRef(child), VType::ObjectRef(parent)) => {
                // `child` / `parent` are `&Arc<str>`; deref to `&str` for
                // the `ClassHierarchy` trait methods.
                //
                // SOUNDNESS (RVERIF.3): an unresolved reference type must
                // NOT make the assignment silently pass. The previous
                // `!is_resolvable(parent) || !is_resolvable(child)` arms
                // were a general verification escape hatch — ANY
                // reference-to-reference assignment was accepted whenever
                // either side happened to be unloaded, which a crafted
                // class file can trivially arrange to bypass the type
                // checker entirely. Per JVMS §4.10.1.2 the verifier must
                // be conservative: when it cannot prove the subtype
                // relationship it rejects the class. The runtime
                // `checkcast` / `invoke*` resolution is a defence in
                // depth, not a substitute for verification.
                hierarchy.is_subclass(child, parent)
                    || is_known_jdk_subtype(child, parent)
                    || hierarchy.is_interface(parent)
                    || is_known_jdk_interface(parent)
                    || hierarchy.is_fallback_merge(child)
            }

            // Array to Object: all arrays are subclasses of java/lang/Object.
            // Also: arrays are reference types; if the target is any
            // interface, accept it under the JVMS verifier relaxation —
            // runtime will enforce the actual type.
            (VType::ArrayRef(_), VType::ObjectRef(parent)) => {
                &**parent == "java/lang/Object"
                    || &**parent == "java/io/Serializable"
                    || &**parent == "java/lang/Cloneable"
                    || hierarchy.is_interface(parent)
                    || is_known_jdk_interface(parent)
            }

            // Array covariance for reference arrays
            (VType::ArrayRef(a), VType::ArrayRef(b)) => array_is_assignable(a, b, hierarchy),

            // UninitializedThis → UninitializedThis (equality handled above)
            // Uninitialized(n) → Uninitialized(n) (equality handled above)

            // SOUNDNESS (RVERIF.3): an uninitialized object is NOT
            // assignable where an initialized reference is expected. This
            // is the classic uninitialized-object attack: if
            // `UninitializedThis` / `Uninitialized(n)` were assignable to
            // an `ObjectRef`, bytecode could pass an object to any method
            // or store it to any field *before* its `<init>` has run,
            // observing or corrupting a partially-constructed instance.
            //
            // Per JVMS §4.10.1.2 / §4.10.1.9 an uninitialized type is only
            // compatible with itself; it becomes an `ObjectRef` only after
            // the verifier sees the matching `invokespecial <init>` (see
            // `replace_vtype_in_frame` in `verify_insn.rs`). A
            // StackMapTable frame that legitimately declares the resolved
            // type at a merge point will only be reached *after* that
            // initialization has been applied, so the equality check at
            // the top of this function already covers the sound cases.
            // The arms accepting `Uninitialized*` → `ObjectRef` are
            // therefore removed.
            _ => false,
        }
    }

    /// Merge two verification types, finding their common supertype.
    ///
    /// Used at control flow merge points where two execution paths meet.
    /// Returns the least upper bound in the verification type lattice.
    pub fn merge(&self, other: &VType, hierarchy: &dyn ClassHierarchy) -> VType {
        // Same type → itself
        if self == other {
            return self.clone();
        }

        // A hierarchy's own reference merge (the libverify-rules walk's
        // arrays; interpreter round i5 wave 112, lane L7).
        if matches!(self, VType::ObjectRef(_) | VType::ArrayRef(_))
            && matches!(other, VType::ObjectRef(_) | VType::ArrayRef(_))
        {
            if let Some(merged) = hierarchy.merge_references(self, other) {
                return merged;
            }
        }

        match (self, other) {
            // Null merged with any reference type → the reference type
            (VType::Null, ref_type) if ref_type.is_reference() => ref_type.clone(),
            (ref_type, VType::Null) if ref_type.is_reference() => ref_type.clone(),

            // Two object references → common superclass
            (VType::ObjectRef(a), VType::ObjectRef(b)) => {
                VType::ObjectRef(hierarchy.merged_class_name(a, b))
            }

            // Array + Object → Object
            (VType::ArrayRef(_), VType::ObjectRef(_))
            | (VType::ObjectRef(_), VType::ArrayRef(_)) => {
                VType::ObjectRef(Arc::from("java/lang/Object"))
            }

            // Two arrays → merge element types if both are reference arrays;
            // otherwise Object
            (VType::ArrayRef(a), VType::ArrayRef(b)) => merge_arrays(a, b, hierarchy),

            // Incompatible types → Top
            _ => VType::Top,
        }
    }
}

// ---------------------------------------------------------------------------
// Array type helpers
// ---------------------------------------------------------------------------

/// The reference types the instruction effects mint over and over (`ldc` of a
/// string or class, `athrow`'s `Throwable`, the primitive array types of
/// `newarray`, `Object` for an untyped `aaload`).
///
/// SBR round 5, lane verify: each of those used to be `Arc::from("...")`, a
/// heap allocation and copy per instruction. They are per THREAD (`Arc`
/// clones touch only this thread's counters, so verifier threads do not
/// contend on one cache line), created on first use.
pub(crate) struct WellKnownTypes {
    pub object: Arc<str>,
    pub string: Arc<str>,
    pub class: Arc<str>,
    pub throwable: Arc<str>,
    pub method_handle: Arc<str>,
    pub method_type: Arc<str>,
    /// `[Z [C [F [D [B [S [I [J`, indexed by `atype - 4`.
    pub prim_arrays: [Arc<str>; 8],
}

thread_local! {
    static WELL_KNOWN: WellKnownTypes = WellKnownTypes {
        object: Arc::from("java/lang/Object"),
        string: Arc::from("java/lang/String"),
        class: Arc::from("java/lang/Class"),
        throwable: Arc::from("java/lang/Throwable"),
        method_handle: Arc::from("java/lang/invoke/MethodHandle"),
        method_type: Arc::from("java/lang/invoke/MethodType"),
        prim_arrays: [
            Arc::from("[Z"),
            Arc::from("[C"),
            Arc::from("[F"),
            Arc::from("[D"),
            Arc::from("[B"),
            Arc::from("[S"),
            Arc::from("[I"),
            Arc::from("[J"),
        ],
    };
}

/// Run `f` on this thread's [`WellKnownTypes`].
#[inline]
pub(crate) fn with_well_known<R>(f: impl FnOnce(&WellKnownTypes) -> R) -> R {
    WELL_KNOWN.with(f)
}

/// How many local-variable slots the parameters of method descriptor
/// `descriptor` occupy (a `long`/`double` counts two), without building the
/// parameter types. Same answer as summing over
/// [`param_types_from_descriptor`]: it stops at the same malformed piece, and a
/// piece is category-2 exactly when it starts with `J` or `D`.
pub(crate) fn descriptor_param_slots(descriptor: &str) -> usize {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return 0;
    }
    let mut slots = 0usize;
    let mut i = 1usize;
    while i < bytes.len() && bytes[i] != b')' {
        let Some(n) = field_descriptor_len(bytes, i) else {
            break;
        };
        if descriptor.get(i..i + n).is_none() {
            break;
        }
        slots += if matches!(bytes[i], b'J' | b'D') {
            2
        } else {
            1
        };
        i += n;
    }
    slots
}

/// Check if array type `child_desc` is assignable to array type `parent_desc`.
///
/// Arrays are covariant for reference element types:
/// - `[Ljava/lang/String;` is assignable to `[Ljava/lang/Object;`
/// - `[I` is NOT assignable to `[J` (primitive arrays are invariant)
/// - `[[I` is assignable to `[Ljava/lang/Object;` (array of array → array of Object)
fn array_is_assignable(
    child_desc: &str,
    parent_desc: &str,
    hierarchy: &dyn ClassHierarchy,
) -> bool {
    if child_desc == parent_desc {
        return true;
    }

    // Both must start with '['. A malformed descriptor that does not
    // (e.g. the bare string "[" with nothing after it, or an empty
    // string) yields `None` element descriptors — treat as not
    // assignable so verification rejects the class instead of panicking
    // on the slice.
    let child_elem = match child_desc.strip_prefix('[') {
        Some(e) => e,
        None => return false,
    };
    let parent_elem = match parent_desc.strip_prefix('[') {
        Some(e) => e,
        None => return false,
    };

    // If both are reference arrays, check element type assignability
    match (
        child_elem.as_bytes().first(),
        parent_elem.as_bytes().first(),
    ) {
        // Both reference arrays (L or [)
        (Some(b'L'), Some(b'L')) => {
            // Strip 'L'..';' with bounds checks; a missing ';'
            // terminator is malformed → not assignable, no panic.
            let (Some(child_class), Some(parent_class)) = (
                strip_object_descriptor(child_elem),
                strip_object_descriptor(parent_elem),
            ) else {
                return false;
            };
            // RVERIF.2: same JDK-interface relaxation as the scalar
            // ObjectRef arm (covers e.g. `[List` -> `[Collection` when
            // both element types are loaded as flag-less stubs).
            //
            // SOUNDNESS (RVERIF.3): the "unresolved → accept" leniency
            // was removed here for the same reason as the scalar
            // `ObjectRef` arm — an unloaded element type must not make an
            // array-covariance assignment pass unchecked. When the
            // verifier cannot prove the element subtype relationship it
            // rejects the class (conservative, per JVMS §4.10.1.2).
            hierarchy.is_subclass(child_class, parent_class)
                || hierarchy.is_interface(parent_class)
                || is_known_jdk_interface(parent_class)
        }
        // Both nested arrays
        (Some(b'['), Some(b'[')) => array_is_assignable(child_elem, parent_elem, hierarchy),
        // Nested array assignable to Object array
        (Some(b'['), Some(b'L')) => {
            let Some(parent_class) = strip_object_descriptor(parent_elem) else {
                return false; // malformed L-type → not assignable
            };
            parent_class == "java/lang/Object"
                || parent_class == "java/io/Serializable"
                || parent_class == "java/lang/Cloneable"
        }
        // Primitive arrays: only equal types (handled by equality check above)
        _ => false,
    }
}

/// Strip the `L` prefix and `;` suffix of an object field descriptor,
/// returning the internal class name. Returns `None` for a malformed
/// descriptor (no `L` prefix or no `;` terminator) so callers can reject
/// it instead of panicking on an out-of-bounds slice.
fn strip_object_descriptor(desc: &str) -> Option<&str> {
    desc.strip_prefix('L').and_then(|s| s.strip_suffix(';'))
}

/// Merge two array types.
fn merge_arrays(a: &str, b: &str, hierarchy: &dyn ClassHierarchy) -> VType {
    if a == b {
        return VType::ArrayRef(Arc::from(a));
    }

    // Both must start with '['. A malformed descriptor that does not
    // (e.g. the bare string "[" or an empty string) cannot be merged as
    // an array — fall back to Object instead of panicking on the slice.
    let (Some(a_elem), Some(b_elem)) = (a.strip_prefix('['), b.strip_prefix('[')) else {
        return VType::ObjectRef(Arc::from("java/lang/Object"));
    };

    match (a_elem.as_bytes().first(), b_elem.as_bytes().first()) {
        // Both reference arrays
        (Some(b'L'), Some(b'L')) => {
            // Strip 'L'..';' with bounds checks; a missing ';'
            // terminator is malformed → fall back to Object, no panic.
            let (Some(a_class), Some(b_class)) = (
                strip_object_descriptor(a_elem),
                strip_object_descriptor(b_elem),
            ) else {
                return VType::ObjectRef(Arc::from("java/lang/Object"));
            };
            let common = hierarchy.common_superclass(a_class, b_class);
            VType::ArrayRef(Arc::from(format!("[L{common};").as_str()))
        }
        // Both nested arrays
        (Some(b'['), Some(b'[')) => match merge_arrays(a_elem, b_elem, hierarchy) {
            VType::ArrayRef(inner) => VType::ArrayRef(Arc::from(format!("[{inner}").as_str())),
            _ => VType::ObjectRef(Arc::from("java/lang/Object")),
        },
        // Otherwise → Object (arrays of different primitive types, etc.)
        _ => VType::ObjectRef(Arc::from("java/lang/Object")),
    }
}

// ---------------------------------------------------------------------------
// RVERIF.4 — name-based JDK superclass fallback
// ---------------------------------------------------------------------------

/// Return `true` for narrow bootstrap superclass edges that synthetic-stub
/// hierarchy metadata can fail to prove during early bootstrap.
///
/// This is intentionally much smaller than [`is_known_jdk_interface`]: these
/// are concrete class-to-superclass relationships, so every accepted edge must
/// be an actual inheritance relation.
pub(crate) fn is_known_jdk_subtype(child: &str, parent: &str) -> bool {
    matches!(
        (child, parent),
        ("java/security/BasicPermission", "java/security/Permission")
            | (
                "java/lang/RuntimePermission",
                "java/security/BasicPermission"
            )
            | ("java/lang/RuntimePermission", "java/security/Permission")
    )
}

// ---------------------------------------------------------------------------
// RVERIF.2 — name-based JDK interface fallback
// ---------------------------------------------------------------------------

/// Return `true` if `name` is a well-known JDK interface.
///
/// Per JVMS §4.10.1.2 the bytecode verifier does not distinguish interface
/// types from `java.lang.Object`: any reference is assignable to any
/// interface at verify time, with runtime checkcast / invokeinterface
/// enforcing the actual type.
///
/// Our [`ClassHierarchy::is_interface`] consults the `INTERFACE` access
/// flag on the loaded class, which works for real classfiles. However our
/// synthetic-stub generator (used when a JDK class is referenced before
/// its `.class` file is read) flags every stub as `PUBLIC | SUPER` unless
/// the name happens to match the heuristic `name.contains('$') ||
/// name.ends_with("able")`. That heuristic mis-classifies many JDK
/// interfaces (`Collection`, `List`, `Map`, `Set`, `Iterator`, ...) as
/// classes — which then makes assignments like `List` → `Collection`
/// fail verification when both are stubbed.
///
/// This static name table is the verifier's safety net. It is consulted
/// only as the last branch of the assignability check, after the loaded
/// hierarchy and the `INTERFACE` flag have already said "no". Adding a
/// name here cannot accept any assignment that JVMS would reject — the
/// entries are all genuinely interfaces in the JDK.
pub(crate) fn is_known_jdk_interface(name: &str) -> bool {
    matches!(
        name,
        // java.lang
        "java/lang/CharSequence"
        | "java/lang/Comparable"
        | "java/lang/Iterable"
        | "java/lang/Readable"
        | "java/lang/Appendable"
        | "java/lang/AutoCloseable"
        | "java/lang/Runnable"
        | "java/lang/Cloneable"
        | "java/lang/constant/Constable"
        | "java/lang/constant/ConstantDesc"
        | "java/lang/reflect/AnnotatedElement"
        | "java/lang/reflect/GenericDeclaration"
        | "java/lang/reflect/Member"
        | "java/lang/reflect/Type"
        | "java/lang/reflect/InvocationHandler"
        | "java/lang/annotation/Annotation"
        // java.io
        | "java/io/Serializable"
        | "java/io/Closeable"
        | "java/io/Flushable"
        | "java/io/Externalizable"
        | "java/io/DataInput"
        | "java/io/DataOutput"
        | "java/io/ObjectInput"
        | "java/io/ObjectOutput"
        | "java/io/ObjectStreamConstants"
        // java.util — collection framework
        | "java/util/Collection"
        | "java/util/List"
        | "java/util/Set"
        | "java/util/Map"
        | "java/util/Map$Entry"
        | "java/util/SortedMap"
        | "java/util/SortedSet"
        | "java/util/NavigableMap"
        | "java/util/NavigableSet"
        | "java/util/SequencedCollection"
        | "java/util/SequencedMap"
        | "java/util/SequencedSet"
        | "java/util/Queue"
        | "java/util/Deque"
        | "java/util/Iterator"
        | "java/util/ListIterator"
        | "java/util/Enumeration"
        | "java/util/Comparator"
        | "java/util/RandomAccess"
        | "java/util/EventListener"
        | "java/util/Spliterator"
        // java.util.concurrent
        | "java/util/concurrent/BlockingQueue"
        | "java/util/concurrent/BlockingDeque"
        | "java/util/concurrent/ConcurrentMap"
        | "java/util/concurrent/ConcurrentNavigableMap"
        | "java/util/concurrent/Callable"
        | "java/util/concurrent/Future"
        | "java/util/concurrent/Executor"
        | "java/util/concurrent/ExecutorService"
        | "java/util/concurrent/ScheduledExecutorService"
        | "java/util/concurrent/CompletionStage"
        | "java/util/concurrent/CompletionService"
        | "java/util/concurrent/RunnableFuture"
        | "java/util/concurrent/RunnableScheduledFuture"
        // java.util.function
        | "java/util/function/Function"
        | "java/util/function/BiFunction"
        | "java/util/function/Consumer"
        | "java/util/function/BiConsumer"
        | "java/util/function/Predicate"
        | "java/util/function/BiPredicate"
        | "java/util/function/Supplier"
        | "java/util/function/UnaryOperator"
        | "java/util/function/BinaryOperator"
        | "java/util/function/IntFunction"
        | "java/util/function/IntPredicate"
        | "java/util/function/IntConsumer"
        | "java/util/function/IntSupplier"
        | "java/util/function/IntUnaryOperator"
        | "java/util/function/IntBinaryOperator"
        | "java/util/function/IntToLongFunction"
        | "java/util/function/IntToDoubleFunction"
        | "java/util/function/LongFunction"
        | "java/util/function/LongPredicate"
        | "java/util/function/LongConsumer"
        | "java/util/function/LongSupplier"
        | "java/util/function/LongUnaryOperator"
        | "java/util/function/LongBinaryOperator"
        | "java/util/function/LongToIntFunction"
        | "java/util/function/LongToDoubleFunction"
        | "java/util/function/DoubleFunction"
        | "java/util/function/DoublePredicate"
        | "java/util/function/DoubleConsumer"
        | "java/util/function/DoubleSupplier"
        | "java/util/function/DoubleUnaryOperator"
        | "java/util/function/DoubleBinaryOperator"
        | "java/util/function/DoubleToIntFunction"
        | "java/util/function/DoubleToLongFunction"
        | "java/util/function/ToIntFunction"
        | "java/util/function/ToLongFunction"
        | "java/util/function/ToDoubleFunction"
        | "java/util/function/ToIntBiFunction"
        | "java/util/function/ToLongBiFunction"
        | "java/util/function/ToDoubleBiFunction"
        | "java/util/function/ObjIntConsumer"
        | "java/util/function/ObjLongConsumer"
        | "java/util/function/ObjDoubleConsumer"
        | "java/util/function/BooleanSupplier"
        // java.util.stream
        | "java/util/stream/BaseStream"
        | "java/util/stream/Stream"
        | "java/util/stream/IntStream"
        | "java/util/stream/LongStream"
        | "java/util/stream/DoubleStream"
        | "java/util/stream/Collector"
        // java.lang.invoke
        | "java/lang/invoke/MethodHandleInfo"
        // java.security
        | "java/security/Principal"
        | "java/security/PrivilegedAction"
        | "java/security/PrivilegedExceptionAction"
        // java.net
        | "java/net/SocketOption"
        | "java/net/SocketImplFactory"
        // java.nio
        | "java/nio/channels/Channel"
        | "java/nio/channels/ReadableByteChannel"
        | "java/nio/channels/WritableByteChannel"
        | "java/nio/channels/ByteChannel"
        | "java/nio/channels/SeekableByteChannel"
        | "java/nio/channels/InterruptibleChannel"
        | "java/nio/channels/AsynchronousChannel"
        | "java/nio/file/Path"
        | "java/nio/file/Watchable"
        // java.time
        | "java/time/temporal/Temporal"
        | "java/time/temporal/TemporalAccessor"
        | "java/time/temporal/TemporalAdjuster"
        | "java/time/temporal/TemporalAmount"
        | "java/time/temporal/TemporalField"
        | "java/time/temporal/TemporalUnit"
        | "java/time/chrono/Chronology"
        | "java/time/chrono/ChronoLocalDate"
        | "java/time/chrono/ChronoLocalDateTime"
        | "java/time/chrono/ChronoZonedDateTime"
    )
}

// ---------------------------------------------------------------------------
// Descriptor well-formedness (JVMS §4.3.2 / §4.3.3)
// ---------------------------------------------------------------------------

/// Length in bytes of the field descriptor starting at `bytes[at]`, or `None`
/// when the bytes at that position are not a well-formed field descriptor.
///
/// SECURITY: this is the single place that decides how far a descriptor
/// extends. Every caller derives its slice bounds from the returned length, so
/// a malformed descriptor produces `None` (→ rejection) instead of an
/// out-of-range slice. The previous ad-hoc `find(';').unwrap_or(len - i)`
/// arithmetic in [`param_types_from_descriptor`] computed an end index of
/// `len + 1` for an unterminated `L` type and panicked on the slice — reachable
/// from any `NameAndType` descriptor in an attacker-supplied constant pool.
fn field_descriptor_len(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = at;
    let mut dims = 0usize;
    while i < bytes.len() && bytes[i] == b'[' {
        dims += 1;
        // JVMS §4.4.1: an array type descriptor is limited to 255 dimensions.
        if dims > 255 {
            return None;
        }
        i += 1;
    }
    let tag = *bytes.get(i)?;
    match tag {
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => Some(i + 1 - at),
        b'L' => {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b';' {
                // JVMS §4.2.1: an internal-form class name never contains
                // `.`, `[` or `;`.
                if bytes[j] == b'.' || bytes[j] == b'[' {
                    return None;
                }
                j += 1;
            }
            if j >= bytes.len() || j == start {
                // Unterminated, or `L;` with an empty class name.
                return None;
            }
            Some(j + 1 - at)
        }
        _ => None,
    }
}

/// Is `descriptor` a well-formed field descriptor (JVMS §4.3.2)?
///
/// Used by the verifier's constant-pool cross-checks: a `Fieldref`'s
/// `NameAndType` must carry a field descriptor, and a malformed one must be a
/// `VerifyError` rather than silently degrading to [`VType::Top`] (which the
/// assignability lattice accepts from *any* value, so a bad descriptor on a
/// `putstatic` would pop an arbitrary operand unchecked).
pub fn is_valid_field_descriptor(descriptor: &str) -> bool {
    let bytes = descriptor.as_bytes();
    matches!(field_descriptor_len(bytes, 0), Some(n) if n == bytes.len())
}

/// Is `descriptor` a well-formed method descriptor (JVMS §4.3.3)?
///
/// Requires the `( … )` envelope, well-formed parameter descriptors, and a
/// return descriptor that is either `V` or a well-formed field descriptor.
pub fn is_valid_method_descriptor(descriptor: &str) -> bool {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return false;
    }
    let mut i = 1usize;
    while i < bytes.len() && bytes[i] != b')' {
        match field_descriptor_len(bytes, i) {
            Some(n) => i += n,
            None => return false,
        }
    }
    if i >= bytes.len() {
        return false; // no closing ')'
    }
    i += 1;
    if bytes.get(i) == Some(&b'V') {
        return i + 1 == bytes.len();
    }
    matches!(field_descriptor_len(bytes, i), Some(n) if i + n == bytes.len())
}

/// Parse a method descriptor's return type into a VType.
///
/// Returns `None` for void (`V`).
pub fn return_type_from_descriptor(descriptor: &str) -> Option<VType> {
    // Find the ')' that separates params from return type
    let ret_start = descriptor.rfind(')').map(|i| i + 1)?;
    let ret_desc = &descriptor[ret_start..];

    if ret_desc == "V" {
        None
    } else {
        Some(VType::from_field_descriptor(ret_desc))
    }
}

/// Parse method parameter types from a descriptor.
///
/// E.g., `"(ILjava/lang/String;[D)V"` → `[Int, ObjectRef("java/lang/String"), ArrayRef("[D")]`
pub fn param_types_from_descriptor(descriptor: &str) -> Vec<VType> {
    let mut types = Vec::new();

    // SECURITY: every slice bound below comes from `field_descriptor_len`,
    // which never reports a length that runs past the end of the descriptor.
    // The previous hand-rolled walk computed `i + semi + 1` from
    // `find(';').unwrap_or(len - i)`, which is `len + 1` for an unterminated
    // `L` type — an out-of-range slice, i.e. a verifier panic driven straight
    // from an attacker-supplied `NameAndType` descriptor.
    //
    // A malformed descriptor stops the walk (rather than skipping the offending
    // byte and continuing) so the caller sees a *short* parameter list. Callers
    // that make a load decision reject malformed descriptors outright via
    // [`is_valid_method_descriptor`]; stopping here is the fail-safe for the
    // remaining diagnostic callers.
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return types; // malformed: no '(' — no parameters
    }
    let mut i = 1usize;
    while i < bytes.len() && bytes[i] != b')' {
        let Some(n) = field_descriptor_len(bytes, i) else {
            break;
        };
        let Some(piece) = descriptor.get(i..i + n) else {
            break;
        };
        types.push(VType::from_field_descriptor(piece));
        i += n;
    }

    types
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
