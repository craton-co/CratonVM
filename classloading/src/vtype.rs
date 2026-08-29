// SPDX-License-Identifier: Apache-2.0
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

        match (self, other) {
            // Null merged with any reference type → the reference type
            (VType::Null, ref_type) if ref_type.is_reference() => ref_type.clone(),
            (ref_type, VType::Null) if ref_type.is_reference() => ref_type.clone(),

            // Two object references → common superclass
            (VType::ObjectRef(a), VType::ObjectRef(b)) => {
                VType::ObjectRef(Arc::from(hierarchy.common_superclass(a, b).as_str()))
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
            | (
                "org/jboss/as/controller/security/ControllerPermission",
                "java/security/BasicPermission"
            )
            | (
                "org/jboss/as/controller/security/ControllerPermission",
                "java/security/Permission"
            )
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
mod tests {
    use super::*;

    /// Mock class hierarchy for testing.
    struct MockHierarchy;

    impl ClassHierarchy for MockHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            // Simple hierarchy: String <: Object, ArrayList <: Object, Integer <: Number <: Object
            if child == parent {
                return true;
            }
            // Every reference type (including interfaces) is a subclass of
            // java.lang.Object.  The real ClassStoreHierarchy mirrors this.
            if parent == "java/lang/Object" {
                return true;
            }
            matches!(
                (child, parent),
                ("java/lang/String", "java/lang/Object")
                    | ("java/util/ArrayList", "java/lang/Object")
                    | ("java/lang/Integer", "java/lang/Number")
                    | ("java/lang/Integer", "java/lang/Object")
                    | ("java/lang/Number", "java/lang/Object")
            )
        }

        fn common_superclass(&self, a: &str, b: &str) -> String {
            if a == b {
                return a.to_string();
            }
            if self.is_subclass(a, b) {
                return b.to_string();
            }
            if self.is_subclass(b, a) {
                return a.to_string();
            }
            // For our mock, everything meets at Object
            "java/lang/Object".to_string()
        }

        fn is_interface(&self, name: &str) -> bool {
            name == "java/io/Serializable" || name == "java/lang/Cloneable"
        }
    }

    // --- from_field_descriptor ---

    #[test]
    fn from_descriptor_int() {
        assert_eq!(VType::from_field_descriptor("I"), VType::Int);
    }

    #[test]
    fn from_descriptor_byte() {
        assert_eq!(VType::from_field_descriptor("B"), VType::Int);
    }

    #[test]
    fn from_descriptor_char() {
        assert_eq!(VType::from_field_descriptor("C"), VType::Int);
    }

    #[test]
    fn from_descriptor_short() {
        assert_eq!(VType::from_field_descriptor("S"), VType::Int);
    }

    #[test]
    fn from_descriptor_boolean() {
        assert_eq!(VType::from_field_descriptor("Z"), VType::Int);
    }

    #[test]
    fn from_descriptor_long() {
        assert_eq!(VType::from_field_descriptor("J"), VType::Long);
    }

    #[test]
    fn from_descriptor_float() {
        assert_eq!(VType::from_field_descriptor("F"), VType::Float);
    }

    #[test]
    fn from_descriptor_double() {
        assert_eq!(VType::from_field_descriptor("D"), VType::Double);
    }

    #[test]
    fn from_descriptor_object() {
        assert_eq!(
            VType::from_field_descriptor("Ljava/lang/String;"),
            VType::ObjectRef(Arc::from("java/lang/String"))
        );
    }

    #[test]
    fn from_descriptor_int_array() {
        assert_eq!(
            VType::from_field_descriptor("[I"),
            VType::ArrayRef(Arc::from("[I"))
        );
    }

    #[test]
    fn from_descriptor_object_array() {
        assert_eq!(
            VType::from_field_descriptor("[Ljava/lang/Object;"),
            VType::ArrayRef(Arc::from("[Ljava/lang/Object;"))
        );
    }

    #[test]
    fn from_descriptor_2d_array() {
        assert_eq!(
            VType::from_field_descriptor("[[I"),
            VType::ArrayRef(Arc::from("[[I"))
        );
    }

    // --- is_category2 ---

    #[test]
    fn category2_long() {
        assert!(VType::Long.is_category2());
    }

    #[test]
    fn category2_double() {
        assert!(VType::Double.is_category2());
    }

    #[test]
    fn category2_int_is_not() {
        assert!(!VType::Int.is_category2());
    }

    // --- is_reference ---

    #[test]
    fn is_reference_object() {
        assert!(VType::ObjectRef(Arc::from("Foo")).is_reference());
    }

    #[test]
    fn is_reference_null() {
        assert!(VType::Null.is_reference());
    }

    #[test]
    fn is_reference_array() {
        assert!(VType::ArrayRef(Arc::from("[I")).is_reference());
    }

    #[test]
    fn is_reference_int_is_not() {
        assert!(!VType::Int.is_reference());
    }

    // --- is_assignable_to ---

    #[test]
    fn assignable_same_type() {
        let h = MockHierarchy;
        assert!(VType::Int.is_assignable_to(&VType::Int, &h));
        assert!(VType::Long.is_assignable_to(&VType::Long, &h));
    }

    #[test]
    fn null_assignable_to_object() {
        let h = MockHierarchy;
        assert!(VType::Null.is_assignable_to(&VType::ObjectRef(Arc::from("java/lang/Object")), &h));
    }

    #[test]
    fn null_assignable_to_array() {
        let h = MockHierarchy;
        assert!(VType::Null.is_assignable_to(&VType::ArrayRef(Arc::from("[I")), &h));
    }

    #[test]
    fn subclass_assignable_to_superclass() {
        let h = MockHierarchy;
        assert!(VType::ObjectRef(Arc::from("java/lang/String"))
            .is_assignable_to(&VType::ObjectRef(Arc::from("java/lang/Object")), &h));
    }

    #[test]
    fn superclass_not_assignable_to_subclass() {
        let h = MockHierarchy;
        assert!(!VType::ObjectRef(Arc::from("java/lang/Object"))
            .is_assignable_to(&VType::ObjectRef(Arc::from("java/lang/String")), &h));
    }

    #[test]
    fn array_assignable_to_object() {
        let h = MockHierarchy;
        assert!(VType::ArrayRef(Arc::from("[I"))
            .is_assignable_to(&VType::ObjectRef(Arc::from("java/lang/Object")), &h));
    }

    #[test]
    fn array_assignable_to_serializable() {
        let h = MockHierarchy;
        assert!(VType::ArrayRef(Arc::from("[I"))
            .is_assignable_to(&VType::ObjectRef(Arc::from("java/io/Serializable")), &h));
    }

    #[test]
    fn covariant_reference_array() {
        let h = MockHierarchy;
        // [String is assignable to [Object
        assert!(VType::ArrayRef(Arc::from("[Ljava/lang/String;"))
            .is_assignable_to(&VType::ArrayRef(Arc::from("[Ljava/lang/Object;")), &h));
    }

    #[test]
    fn primitive_array_not_covariant() {
        let h = MockHierarchy;
        assert!(!VType::ArrayRef(Arc::from("[I"))
            .is_assignable_to(&VType::ArrayRef(Arc::from("[J")), &h));
    }

    #[test]
    fn int_not_assignable_to_long() {
        let h = MockHierarchy;
        assert!(!VType::Int.is_assignable_to(&VType::Long, &h));
    }

    #[test]
    fn top_not_assignable_to_int() {
        let h = MockHierarchy;
        assert!(!VType::Top.is_assignable_to(&VType::Int, &h));
    }

    #[test]
    fn any_type_assignable_to_top() {
        // Per JVMS 4.10.1.2, Top is the top of the verification type lattice.
        // StackMapTable frames may declare a slot as Top to indicate the value
        // is unused past this merge point; any incoming type must widen to Top.
        let h = MockHierarchy;
        assert!(VType::Int.is_assignable_to(&VType::Top, &h));
        assert!(VType::Long.is_assignable_to(&VType::Top, &h));
        assert!(VType::Null.is_assignable_to(&VType::Top, &h));
        assert!(VType::ObjectRef(Arc::from("java/lang/String")).is_assignable_to(&VType::Top, &h));
        assert!(VType::ArrayRef(Arc::from("[I")).is_assignable_to(&VType::Top, &h));
    }

    // --- Interface relaxation (JVMS §4.10.1.2) ----------------------------
    //
    // The verifier does not distinguish between interface types and
    // java.lang.Object in its tracking of reference types. Therefore:
    //   * Object.is_assignable_to(SomeInterface) → true
    //   * SomeClass.is_assignable_to(SomeInterface-it-does-not-implement)
    //       → true
    //   * SomeInterface.is_assignable_to(Object) → true (always did)
    // Runtime checkcast / invokeinterface enforce the actual type.

    #[test]
    fn object_assignable_to_interface() {
        // Object.is_assignable_to(SomeInterface) → true (relaxation rule).
        let h = MockHierarchy;
        let obj = VType::ObjectRef(Arc::from("java/lang/Object"));
        let iface = VType::ObjectRef(Arc::from("java/io/Serializable"));
        assert!(obj.is_assignable_to(&iface, &h));
    }

    #[test]
    fn interface_assignable_to_object() {
        // SomeInterface.is_assignable_to(Object) → true (always was).
        let h = MockHierarchy;
        let iface = VType::ObjectRef(Arc::from("java/io/Serializable"));
        let obj = VType::ObjectRef(Arc::from("java/lang/Object"));
        assert!(iface.is_assignable_to(&obj, &h));
    }

    #[test]
    fn class_assignable_to_unrelated_interface() {
        // SomeClass.is_assignable_to(Interface it doesn't implement) → true
        // at verify time per JVMS §4.10.1.2 relaxation.
        //
        // In MockHierarchy, "java/lang/String" is NOT declared as
        // implementing "java/lang/Cloneable" via is_subclass, yet verify-
        // time assignment must still succeed because the target is an
        // interface.
        let h = MockHierarchy;
        let s = VType::ObjectRef(Arc::from("java/lang/String"));
        let iface = VType::ObjectRef(Arc::from("java/lang/Cloneable"));
        // Sanity: MockHierarchy does NOT model String <: Cloneable.
        assert!(!h.is_subclass("java/lang/String", "java/lang/Cloneable"));
        // Yet is_assignable_to must accept it (interface relaxation).
        assert!(s.is_assignable_to(&iface, &h));
    }

    #[test]
    fn array_assignable_to_arbitrary_interface() {
        // Arrays are reference types; assignment to ANY interface is
        // accepted at verify time. The existing Serializable/Cloneable
        // paths were already accepted; this ensures generic interface
        // targets also succeed via is_interface().
        struct OnlyInterfaceHierarchy;
        impl ClassHierarchy for OnlyInterfaceHierarchy {
            fn is_subclass(&self, c: &str, p: &str) -> bool {
                c == p || p == "java/lang/Object"
            }
            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }
            fn is_interface(&self, name: &str) -> bool {
                name == "my/pkg/IFoo"
            }
        }
        let h = OnlyInterfaceHierarchy;
        let arr = VType::ArrayRef(Arc::from("[I"));
        let iface = VType::ObjectRef(Arc::from("my/pkg/IFoo"));
        assert!(arr.is_assignable_to(&iface, &h));
    }

    // --- RVERIF.2: known-JDK-interface fallback ----------------------------
    //
    // When a JDK interface is loaded as a synthetic stub (because its real
    // .class wasn't yet read at verify time), the stub heuristic can flag
    // it as PUBLIC|SUPER instead of INTERFACE — which makes the existing
    // `is_interface(parent)` relaxation miss and breaks legitimate
    // List → Collection / ArrayList → Iterable assignments. The
    // `is_known_jdk_interface` static table is the verifier's safety net
    // for those cases.

    /// Hierarchy that mimics the failure mode: classes are loaded but
    /// `is_subclass` does not see the interface relationship and
    /// `is_interface` returns `false` for everything (because the stub
    /// access flags are wrong).
    struct StubHierarchyMissingInterfaceFlag;
    impl ClassHierarchy for StubHierarchyMissingInterfaceFlag {
        fn is_subclass(&self, c: &str, p: &str) -> bool {
            // Same class or anything-to-Object only.
            c == p || p == "java/lang/Object"
        }
        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }
        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    #[test]
    fn list_assignable_to_collection_via_jdk_table() {
        // RVERIF.2 regression: this is the canonical case from
        // org/jboss/modules/Module.getResources at bytecode offset 294 —
        // `Collections.enumeration(Collection)` invoked with a List on
        // the stack. With a stub-only hierarchy that has neither the
        // subclass relationship nor the INTERFACE flag, the verifier
        // must still accept the assignment because Collection is a
        // well-known JDK interface.
        let h = StubHierarchyMissingInterfaceFlag;
        let list = VType::ObjectRef(Arc::from("java/util/List"));
        let collection = VType::ObjectRef(Arc::from("java/util/Collection"));

        // Sanity: the hierarchy genuinely cannot prove the relationship.
        assert!(!h.is_subclass("java/util/List", "java/util/Collection"));
        assert!(!h.is_interface("java/util/Collection"));

        // Yet the verifier accepts the assignment.
        assert!(
            list.is_assignable_to(&collection, &h),
            "List must be assignable to Collection (JDK-interface fallback)"
        );
    }

    #[test]
    fn arraylist_assignable_to_iterable_via_jdk_table() {
        // Concrete class to interface, both via JDK fallback.
        let h = StubHierarchyMissingInterfaceFlag;
        let al = VType::ObjectRef(Arc::from("java/util/ArrayList"));
        let iter = VType::ObjectRef(Arc::from("java/lang/Iterable"));
        assert!(al.is_assignable_to(&iter, &h));
    }

    #[test]
    fn runtime_permission_assignable_to_permission_via_jdk_table() {
        // WildFly's JBossThread.onExit calls
        // SecurityManager.checkPermission(Permission) with a RuntimePermission
        // static. When RuntimePermission is still represented by sparse
        // bootstrap metadata, the verifier must still accept this real JDK
        // superclass edge.
        let h = StubHierarchyMissingInterfaceFlag;
        let runtime_permission = VType::ObjectRef(Arc::from("java/lang/RuntimePermission"));
        let permission = VType::ObjectRef(Arc::from("java/security/Permission"));

        assert!(!h.is_subclass("java/lang/RuntimePermission", "java/security/Permission"));
        assert!(runtime_permission.is_assignable_to(&permission, &h));
    }

    #[test]
    fn wildfly_controller_permission_assignable_to_permission_via_bootstrap_table() {
        // WildFly's ModelController.<clinit> initializes ControllerPermission
        // constants and stores them where java.security.Permission is expected.
        // When ControllerPermission/BasicPermission hierarchy metadata is sparse
        // during early host-controller bootstrap, the verifier still needs this
        // real superclass edge.
        let h = StubHierarchyMissingInterfaceFlag;
        let controller_permission = VType::ObjectRef(Arc::from(
            "org/jboss/as/controller/security/ControllerPermission",
        ));
        let basic_permission = VType::ObjectRef(Arc::from("java/security/BasicPermission"));
        let permission = VType::ObjectRef(Arc::from("java/security/Permission"));

        assert!(!h.is_subclass(
            "org/jboss/as/controller/security/ControllerPermission",
            "java/security/Permission"
        ));
        assert!(controller_permission.is_assignable_to(&basic_permission, &h));
        assert!(controller_permission.is_assignable_to(&permission, &h));
    }

    #[test]
    fn unknown_class_not_assignable_to_unknown_class() {
        // Negative control: the JDK fallback must NOT accept arbitrary
        // class-to-class assignments. Only known JDK interfaces relax
        // the check.
        let h = StubHierarchyMissingInterfaceFlag;
        let foo = VType::ObjectRef(Arc::from("com/example/Foo"));
        let bar = VType::ObjectRef(Arc::from("com/example/Bar"));
        assert!(!foo.is_assignable_to(&bar, &h));
    }

    // --- RVERIF.3 soundness regressions -----------------------------------

    #[test]
    fn uninitialized_not_assignable_to_object() {
        // The classic uninitialized-object attack: an uninitialized object
        // must NOT be assignable where an initialized reference is
        // expected (JVMS §4.10.1.2). It becomes an ObjectRef only after
        // the verifier sees the matching invokespecial <init>.
        let h = MockHierarchy;
        let obj = VType::ObjectRef(Arc::from("java/lang/Object"));
        assert!(!VType::UninitializedThis.is_assignable_to(&obj, &h));
        assert!(!VType::Uninitialized(7).is_assignable_to(&obj, &h));
    }

    #[test]
    fn uninitialized_assignable_only_to_itself() {
        // An uninitialized type is compatible with itself (equality) but
        // nothing else among reference types.
        let h = MockHierarchy;
        assert!(VType::UninitializedThis.is_assignable_to(&VType::UninitializedThis, &h));
        assert!(VType::Uninitialized(3).is_assignable_to(&VType::Uninitialized(3), &h));
        assert!(!VType::Uninitialized(3).is_assignable_to(&VType::Uninitialized(4), &h));
    }

    #[test]
    fn unresolved_type_does_not_bypass_assignability() {
        // RVERIF.3: a hierarchy that reports a type as not-yet-resolvable
        // must NOT make an otherwise-unprovable reference assignment pass.
        struct UnresolvedHierarchy;
        impl ClassHierarchy for UnresolvedHierarchy {
            fn is_subclass(&self, c: &str, p: &str) -> bool {
                c == p || p == "java/lang/Object"
            }
            fn common_superclass(&self, _a: &str, _b: &str) -> String {
                "java/lang/Object".to_string()
            }
            fn is_interface(&self, _name: &str) -> bool {
                false
            }
            fn is_resolvable(&self, _name: &str) -> bool {
                false // everything unresolved
            }
        }
        let h = UnresolvedHierarchy;
        let foo = VType::ObjectRef(Arc::from("com/example/Foo"));
        let bar = VType::ObjectRef(Arc::from("com/example/Bar"));
        // Unprovable class-to-class assignment must be rejected even
        // though both types are "unresolved".
        assert!(!foo.is_assignable_to(&bar, &h));
        // Same for reference-array element types.
        let afoo = VType::ArrayRef(Arc::from("[Lcom/example/Foo;"));
        let abar = VType::ArrayRef(Arc::from("[Lcom/example/Bar;"));
        assert!(!afoo.is_assignable_to(&abar, &h));
    }

    #[test]
    fn param_types_from_empty_descriptor_does_not_panic() {
        // RVERIF.3: a zero-length descriptor must not panic on slicing.
        assert!(param_types_from_descriptor("").is_empty());
        assert!(param_types_from_descriptor("(").is_empty());
        assert!(param_types_from_descriptor(")V").is_empty());
    }

    #[test]
    fn known_jdk_interface_recognises_collection_framework() {
        // Spot-check that the table covers the names that
        // Module.getResources / Collections / Stream APIs rely on.
        assert!(is_known_jdk_interface("java/util/Collection"));
        assert!(is_known_jdk_interface("java/util/List"));
        assert!(is_known_jdk_interface("java/util/Map"));
        assert!(is_known_jdk_interface("java/util/Set"));
        assert!(is_known_jdk_interface("java/util/Iterator"));
        assert!(is_known_jdk_interface("java/util/Enumeration"));
        assert!(is_known_jdk_interface("java/lang/Iterable"));
        assert!(is_known_jdk_interface("java/util/function/Function"));
        assert!(is_known_jdk_interface("java/util/stream/Stream"));
        // Negative: a concrete class must NOT be in the table.
        assert!(!is_known_jdk_interface("java/util/ArrayList"));
        assert!(!is_known_jdk_interface("java/lang/Object"));
        assert!(!is_known_jdk_interface("java/lang/String"));
    }

    // --- merge ---

    #[test]
    fn merge_same_type() {
        let h = MockHierarchy;
        assert_eq!(VType::Int.merge(&VType::Int, &h), VType::Int);
    }

    #[test]
    fn merge_null_with_object() {
        let h = MockHierarchy;
        let obj = VType::ObjectRef(Arc::from("java/lang/String"));
        assert_eq!(VType::Null.merge(&obj, &h), obj);
        assert_eq!(obj.merge(&VType::Null, &h), obj);
    }

    #[test]
    fn merge_two_objects() {
        let h = MockHierarchy;
        let s = VType::ObjectRef(Arc::from("java/lang/String"));
        let al = VType::ObjectRef(Arc::from("java/util/ArrayList"));
        assert_eq!(
            s.merge(&al, &h),
            VType::ObjectRef(Arc::from("java/lang/Object"))
        );
    }

    #[test]
    fn merge_subclass_with_superclass() {
        let h = MockHierarchy;
        let integer = VType::ObjectRef(Arc::from("java/lang/Integer"));
        let number = VType::ObjectRef(Arc::from("java/lang/Number"));
        assert_eq!(
            integer.merge(&number, &h),
            VType::ObjectRef(Arc::from("java/lang/Number"))
        );
    }

    #[test]
    fn merge_int_with_long_is_top() {
        let h = MockHierarchy;
        assert_eq!(VType::Int.merge(&VType::Long, &h), VType::Top);
    }

    #[test]
    fn merge_array_with_object_is_object() {
        let h = MockHierarchy;
        let arr = VType::ArrayRef(Arc::from("[I"));
        let obj = VType::ObjectRef(Arc::from("java/lang/String"));
        assert_eq!(
            arr.merge(&obj, &h),
            VType::ObjectRef(Arc::from("java/lang/Object"))
        );
    }

    #[test]
    fn merge_two_reference_arrays() {
        let h = MockHierarchy;
        let a = VType::ArrayRef(Arc::from("[Ljava/lang/String;"));
        let b = VType::ArrayRef(Arc::from("[Ljava/util/ArrayList;"));
        assert_eq!(
            a.merge(&b, &h),
            VType::ArrayRef(Arc::from("[Ljava/lang/Object;"))
        );
    }

    #[test]
    fn merge_different_primitive_arrays() {
        let h = MockHierarchy;
        let a = VType::ArrayRef(Arc::from("[I"));
        let b = VType::ArrayRef(Arc::from("[J"));
        assert_eq!(
            a.merge(&b, &h),
            VType::ObjectRef(Arc::from("java/lang/Object"))
        );
    }

    // --- param_types_from_descriptor ---

    #[test]
    fn param_types_empty() {
        assert!(param_types_from_descriptor("()V").is_empty());
    }

    #[test]
    fn param_types_single_int() {
        assert_eq!(param_types_from_descriptor("(I)V"), vec![VType::Int]);
    }

    #[test]
    fn param_types_mixed() {
        let types = param_types_from_descriptor("(ILjava/lang/String;[DJ)V");
        assert_eq!(
            types,
            vec![
                VType::Int,
                VType::ObjectRef(Arc::from("java/lang/String")),
                VType::ArrayRef(Arc::from("[D")),
                VType::Long,
            ]
        );
    }

    #[test]
    fn param_types_2d_array() {
        let types = param_types_from_descriptor("([[I)V");
        assert_eq!(types, vec![VType::ArrayRef(Arc::from("[[I"))]);
    }

    // --- return_type_from_descriptor ---

    #[test]
    fn return_type_void() {
        assert_eq!(return_type_from_descriptor("()V"), None);
    }

    #[test]
    fn return_type_int() {
        assert_eq!(return_type_from_descriptor("()I"), Some(VType::Int));
    }

    #[test]
    fn return_type_object() {
        assert_eq!(
            return_type_from_descriptor("()Ljava/lang/String;"),
            Some(VType::ObjectRef(Arc::from("java/lang/String")))
        );
    }

    #[test]
    fn return_type_array() {
        assert_eq!(
            return_type_from_descriptor("()[I"),
            Some(VType::ArrayRef(Arc::from("[I")))
        );
    }

    // --- descriptor well-formedness (JVMS §4.3.2 / §4.3.3) ---

    #[test]
    fn valid_field_descriptors_accepted() {
        for d in [
            "I",
            "J",
            "D",
            "F",
            "B",
            "C",
            "S",
            "Z",
            "[I",
            "[[J",
            "Ljava/lang/String;",
            "[Ljava/lang/Object;",
            "[[Ljava/util/Map$Entry;",
        ] {
            assert!(is_valid_field_descriptor(d), "{d} should be valid");
        }
    }

    #[test]
    fn malformed_field_descriptors_rejected() {
        for d in [
            "",                    // empty
            "V",                   // void is not a field type
            "Q",                   // unknown tag
            "L",                   // bare L
            "Ljava/lang/String",   // unterminated
            "L;",                  // empty class name
            "Lja.va/Foo;",         // '.' is illegal in an internal name
            "[",                   // dangling array marker
            "II",                  // trailing garbage
            "Ljava/lang/String;X", // trailing garbage
        ] {
            assert!(!is_valid_field_descriptor(d), "{d} should be rejected");
        }
    }

    #[test]
    fn valid_method_descriptors_accepted() {
        for d in [
            "()V",
            "()I",
            "(I)V",
            "(IJ)Ljava/lang/String;",
            "([[Ljava/lang/Object;D)[I",
            "(Ljava/lang/String;Ljava/lang/String;)Z",
        ] {
            assert!(is_valid_method_descriptor(d), "{d} should be valid");
        }
    }

    #[test]
    fn malformed_method_descriptors_rejected() {
        for d in [
            "",
            "V",
            "()",                   // no return type
            "(I",                   // no ')'
            "I)V",                  // no '('
            "()VV",                 // trailing garbage
            "(Ljava/lang/String)V", // unterminated parameter
            "(Q)V",                 // unknown parameter tag
            "()Q",                  // unknown return tag
        ] {
            assert!(!is_valid_method_descriptor(d), "{d} should be rejected");
        }
    }

    #[test]
    fn unterminated_object_parameter_does_not_panic() {
        // REGRESSION: the old walk computed the parameter's end as
        // `i + find(';').unwrap_or(len - i) + 1`, i.e. `len + 1` when the `;`
        // is missing — an out-of-range slice, reachable from any attacker
        // supplied `NameAndType` descriptor. Must return cleanly instead.
        assert!(param_types_from_descriptor("(Ljava/lang/String)V").is_empty());
        assert!(param_types_from_descriptor("([Ljava/lang/String)V").is_empty());
        assert!(param_types_from_descriptor("(L)V").is_empty());
        assert!(param_types_from_descriptor("([").is_empty());
    }

    #[test]
    fn well_formed_parameters_still_parse() {
        assert_eq!(
            param_types_from_descriptor("(IJLjava/lang/String;[D)V"),
            vec![
                VType::Int,
                VType::Long,
                VType::ObjectRef(Arc::from("java/lang/String")),
                VType::ArrayRef(Arc::from("[D")),
            ]
        );
    }

    #[test]
    fn deeply_nested_array_descriptor_is_bounded() {
        // JVMS §4.4.1 caps array descriptors at 255 dimensions; a longer one is
        // malformed rather than an unbounded recursion / allocation source.
        let ok = format!("{}I", "[".repeat(255));
        let too_deep = format!("{}I", "[".repeat(256));
        assert!(is_valid_field_descriptor(&ok));
        assert!(!is_valid_field_descriptor(&too_deep));
    }
}
