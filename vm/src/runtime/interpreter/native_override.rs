// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! When a registered native wins over real bytecode, and when it must not.
//!
//! This VM ships Rust natives for methods that also have real JDK or library
//! bytecode. Choosing between them is a policy, and the policy is large: one
//! `force_native_over_real_jdk_bytecode` decision function plus about seventy
//! `is_*_native_override` predicates, each naming a specific class or library
//! whose bytecode this VM cannot yet run correctly.
//!
//! Two facts here have live dependents and must not be tidied away:
//!
//! * **A registered native wins over real bytecode unconditionally.** The
//!   predicates decide *which* methods are registered as overrides, not whether
//!   an override applies once it exists.
//! * **A native on an abstract class intercepts every subclass.** That is the
//!   superclass walk in `intercept_force_registered_native`, and several
//!   library shims depend on exactly that reach.
//!
//! The `redefine_immune_*` family is the counterweight: once a class has been
//! redefined, its bytecode is by definition newer than any native this VM
//! shipped for it, so the override has to yield. Getting that backwards means
//! a redefinition silently does nothing.
//!
//! The two test modules moved with the code they test — their `use super::X`
//! names this module now.

use super::*;

/// Whether `(class, method, desc)` is one of the reflection TYPE_USE-annotation
/// methods CratonVM must serve from a Rust native instead of the real JDK
/// bytecode (which decodes type annotations via `getTypeAnnotationBytes0()` —
/// stubbed to null — plus the unexposed `jdk.internal.reflect.ConstantPool`).
///
/// This is the single source of truth for that override set: it is consulted by
/// BOTH dispatch gates — [`force_native_over_real_jdk_bytecode`] (interpreter
/// fast paths) and the `check_override` predicate in
/// `vm_exec.rs::invoke_on_class_shared_inner` (the slow path). Adding a method
/// here makes it win on every path; editing one list and not the other was the
/// original footgun.
pub(crate) fn is_typeuse_annotation_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match class_name {
        "java/lang/reflect/Method" => matches!(
            (method_name, descriptor),
            (
                "getAnnotatedReturnType",
                "()Ljava/lang/reflect/AnnotatedType;"
            ) | (
                "getAnnotatedParameterTypes",
                "()[Ljava/lang/reflect/AnnotatedType;"
            )
        ),
        "java/lang/reflect/Constructor" => {
            (method_name, descriptor)
                == (
                    "getAnnotatedParameterTypes",
                    "()[Ljava/lang/reflect/AnnotatedType;",
                )
        }
        "java/lang/reflect/Parameter" | "java/lang/reflect/Field" => {
            (method_name, descriptor) == ("getAnnotatedType", "()Ljava/lang/reflect/AnnotatedType;")
        }
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl" => matches!(
            (method_name, descriptor),
            (
                "getAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
            ) | ("getAnnotations", "()[Ljava/lang/annotation/Annotation;")
                | (
                    "getDeclaredAnnotations",
                    "()[Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getAnnotatedOwnerType",
                    "()Ljava/lang/reflect/AnnotatedType;"
                )
        ),
        _ => false,
    }
}

/// java.lang.Class methods whose registered natives operate on CratonVM's
/// class-mirror and annotation side tables. Ordinary bytecode invokes already
/// prefer these registrations, but bound virtual method references dispatch
/// through invoke_on_class_shared, whose concrete-bytecode precedence needs
/// an explicit shared gate.
pub(crate) fn is_class_mirror_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/Class"
        && matches!(
            (method_name, descriptor),
            ("getName", "()Ljava/lang/String;")
                | ("isArray", "()Z")
                | ("getComponentType", "()Ljava/lang/Class;")
                | ("componentType", "()Ljava/lang/Class;")
                | ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
                | ("forPrimitiveName", "(Ljava/lang/String;)Ljava/lang/Class;")
                | ("getAnnotations", "()[Ljava/lang/annotation/Annotation;")
                | (
                    "getDeclaredAnnotations",
                    "()[Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getAnnotation",
                    "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getDeclaredAnnotation",
                    "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
                )
                | ("isAnnotationPresent", "(Ljava/lang/Class;)Z")
                | (
                    "getAnnotationsByType",
                    "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getDeclaredAnnotationsByType",
                    "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;"
                )
                | ("getDeclaredFields", "()[Ljava/lang/reflect/Field;")
                | ("getDeclaredFields0", "(Z)[Ljava/lang/reflect/Field;")
                | (
                    "getDeclaredField",
                    "(Ljava/lang/String;)Ljava/lang/reflect/Field;"
                )
        )
}

/// `java.lang.ClassValue#get`/`#remove` (see `classvalue_cache.rs` in
/// native-builtins for the real implementation and why it must be a native
/// override at all — `ClassValue`'s real bytecode depends on CASing a hidden
/// field on `java.lang.Class` via `jdk.internal.misc.Unsafe`, not faithfully
/// reproducible against CratonVM's `Class` mirrors).
///
/// Apache Groovy's `ClassInfo` registry (`ClassInfo.globalClassValue`, a
/// `GroovyClassValueJava7`) is constructed via
/// `GroovyClassValueFactory.createGroovyClassValue(ClassInfo::new)` — a
/// constructor-reference-backed `ComputeValue` lambda — and `ClassInfo`'s own
/// `getClassInfo`/`remove` static methods call `get`/`remove` on it. Ordinary
/// bytecode invokes already prefer the registered native, but this call
/// pattern (through the lambda-backed `ComputeValue` plumbing) can resolve
/// through a dispatch path whose concrete-bytecode precedence needs this
/// explicit shared gate — see core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md Cluster C
/// "Residual 5" (fixed under `--nojit` without this gate; JIT mode still hit
/// the original always-null-returning symptom until this was added).
pub(crate) fn is_classvalue_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let result = class_name == "java/lang/ClassValue"
        && matches!(
            (method_name, descriptor),
            ("get", "(Ljava/lang/Class;)Ljava/lang/Object;") | ("remove", "(Ljava/lang/Class;)V")
        );
    if class_name == "java/lang/ClassValue"
        && cratonvm_types::flags::runtime_var_os("CRATONVM_TRACE_CLASSVALUE").is_some()
    {
        eprintln!(
            "[classvalue-gate] is_classvalue_native_override({class_name}, {method_name}, {descriptor}) -> {result}"
        );
    }
    result
}

pub(crate) fn is_reflection_access_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        (class_name, method_name, descriptor),
        (
            "java/lang/Class",
            "getDeclaredField",
            "(Ljava/lang/String;)Ljava/lang/reflect/Field;"
        ) | (
            "java/lang/reflect/Field",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ) | (
            "java/lang/reflect/Field",
            "set",
            "(Ljava/lang/Object;Ljava/lang/Object;)V"
        ) | ("java/lang/reflect/Field", "getInt", "(Ljava/lang/Object;)I")
            | (
                "java/lang/reflect/Field",
                "getLong",
                "(Ljava/lang/Object;)J"
            )
            | (
                "java/lang/reflect/Field",
                "getFloat",
                "(Ljava/lang/Object;)F"
            )
            | (
                "java/lang/reflect/Field",
                "getDouble",
                "(Ljava/lang/Object;)D"
            )
            | (
                "java/lang/reflect/Field",
                "getBoolean",
                "(Ljava/lang/Object;)Z"
            )
            | (
                "java/lang/reflect/Field",
                "getByte",
                "(Ljava/lang/Object;)B"
            )
            | (
                "java/lang/reflect/Field",
                "getShort",
                "(Ljava/lang/Object;)S"
            )
            | (
                "java/lang/reflect/Field",
                "getChar",
                "(Ljava/lang/Object;)C"
            )
            | (
                "java/lang/reflect/Field",
                "setInt",
                "(Ljava/lang/Object;I)V"
            )
            | (
                "java/lang/reflect/Field",
                "setLong",
                "(Ljava/lang/Object;J)V"
            )
            | (
                "java/lang/reflect/Field",
                "setFloat",
                "(Ljava/lang/Object;F)V"
            )
            | (
                "java/lang/reflect/Field",
                "setDouble",
                "(Ljava/lang/Object;D)V"
            )
            | (
                "java/lang/reflect/Field",
                "setBoolean",
                "(Ljava/lang/Object;Z)V"
            )
            | (
                "java/lang/reflect/Field",
                "setByte",
                "(Ljava/lang/Object;B)V"
            )
            | (
                "java/lang/reflect/Field",
                "setShort",
                "(Ljava/lang/Object;S)V"
            )
            | (
                "java/lang/reflect/Field",
                "setChar",
                "(Ljava/lang/Object;C)V"
            )
            | ("java/lang/reflect/Field", "setAccessible", "(Z)V")
            | (
                "java/lang/reflect/AccessibleObject",
                "setAccessible",
                "(Z)V"
            )
    )
}

pub(crate) fn is_antlr_prediction_context_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let antlr_runtime = class_name.starts_with("org/antlr/v4/runtime/")
        || class_name.starts_with("groovyjarjarantlr4/v4/runtime/");
    if !antlr_runtime {
        return false;
    }
    if class_name == "org/antlr/v4/runtime/CommonTokenFactory" {
        return matches!(
            (method_name, descriptor),
            (
                "create",
                "(Lorg/antlr/v4/runtime/misc/Pair;ILjava/lang/String;IIIII)Lorg/antlr/v4/runtime/CommonToken;"
            ) | (
                "create",
                "(Lorg/antlr/v4/runtime/misc/Pair;ILjava/lang/String;IIIII)Lorg/antlr/v4/runtime/Token;"
            ) | (
                "create",
                "(ILjava/lang/String;)Lorg/antlr/v4/runtime/CommonToken;"
            ) | ("create", "(ILjava/lang/String;)Lorg/antlr/v4/runtime/Token;")
        );
    }
    if class_name == "org/antlr/v4/runtime/CommonToken" {
        return matches!(
            (method_name, descriptor),
            ("getType", "()I")
                | ("setType", "(I)V")
                | ("getText", "()Ljava/lang/String;")
                | ("setText", "(Ljava/lang/String;)V")
                | ("getLine", "()I")
                | ("setLine", "(I)V")
                | ("getCharPositionInLine", "()I")
                | ("setCharPositionInLine", "(I)V")
                | ("getChannel", "()I")
                | ("setChannel", "(I)V")
                | ("getStartIndex", "()I")
                | ("setStartIndex", "(I)V")
                | ("getStopIndex", "()I")
                | ("setStopIndex", "(I)V")
                | ("getTokenIndex", "()I")
                | ("setTokenIndex", "(I)V")
                | ("getTokenSource", "()Lorg/antlr/v4/runtime/TokenSource;")
                | ("getInputStream", "()Lorg/antlr/v4/runtime/CharStream;")
        );
    }
    if class_name.ends_with("/misc/DoubleKeyMap") {
        return matches!(
            (method_name, descriptor),
            ("<init>", "()V")
                | (
                    "get",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                )
                | (
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                )
        );
    }
    if class_name.ends_with("/atn/ATNConfigSet") || class_name.ends_with("/atn/OrderedATNConfigSet")
    {
        return matches!(
            (method_name, descriptor),
            ("add", "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z")
                | (
                    "add",
                    "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/misc/DoubleKeyMap;)Z"
                )
                | ("add", "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z")
                | (
                    "add",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Z"
                )
                | ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name.ends_with("/atn/ATNConfig") {
        return matches!(
            (method_name, descriptor),
            (
                "<init>",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;)V"
            )
                | (
                    "<init>",
                    "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;)V"
                )
                | (
                    "<init>",
                    "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/PredictionContext;)V"
                )
                | (
                    "<init>",
                    "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/SemanticContext;)V"
                )
                | (
                "<init>",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/PredictionContext;Lorg/antlr/v4/runtime/atn/SemanticContext;)V"
            )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)V"
                )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNState;)V"
                )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNState;Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;)V"
                )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNState;Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;)V"
                )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNState;Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;)V"
                )
                | ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("equals", "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z")
                | ("equals", "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z")
        );
    }
    if class_name.ends_with("/atn/LexerATNConfig") {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("equals", "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z")
                | ("equals", "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z")
        );
    }
    if class_name.ends_with("/dfa/DFAState") {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name.ends_with("/atn/SemanticContext") {
        return matches!(
            (method_name, descriptor),
            (
                "and",
                "(Lorg/antlr/v4/runtime/atn/SemanticContext;Lorg/antlr/v4/runtime/atn/SemanticContext;)Lorg/antlr/v4/runtime/atn/SemanticContext;"
            ) | (
                "or",
                "(Lorg/antlr/v4/runtime/atn/SemanticContext;Lorg/antlr/v4/runtime/atn/SemanticContext;)Lorg/antlr/v4/runtime/atn/SemanticContext;"
            ) | (
                "and",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;)Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;"
            ) | (
                "or",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;)Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;"
            )
        );
    }
    if matches!(
        class_name.rsplit('/').next().unwrap_or_default(),
        "SemanticContext$Predicate"
            | "SemanticContext$PrecedencePredicate"
            | "SemanticContext$AND"
            | "SemanticContext$OR"
    ) && class_name.contains("/atn/")
    {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name.ends_with("/atn/ATNState") {
        return matches!(
            (method_name, descriptor),
            ("getNumberOfTransitions", "()I")
                | ("onlyHasEpsilonTransitions", "()Z")
                | ("transition", "(I)Lorg/antlr/v4/runtime/atn/Transition;")
                | (
                    "transition",
                    "(I)Lgroovyjarjarantlr4/v4/runtime/atn/Transition;"
                )
        );
    }
    if matches!(
        class_name.rsplit('/').next().unwrap_or_default(),
        "BasicState"
            | "RuleStartState"
            | "BasicBlockStartState"
            | "PlusBlockStartState"
            | "StarBlockStartState"
            | "TokensStartState"
            | "RuleStopState"
            | "BlockEndState"
            | "StarLoopbackState"
            | "StarLoopEntryState"
            | "PlusLoopbackState"
            | "LoopEndState"
    ) && class_name.contains("/atn/")
    {
        return (method_name, descriptor) == ("getStateType", "()I");
    }
    if class_name.ends_with("/misc/IntervalSet") {
        return (method_name, descriptor) == ("contains", "(I)Z");
    }
    if class_name.ends_with("/atn/ParserATNSimulator") {
        return matches!(
            (method_name, descriptor),
            (
                "canDropLoopEntryEdgeInLeftRecursiveRule",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z"
            ) | (
                "canDropLoopEntryEdgeInLeftRecursiveRule",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z"
            ) | (
                "getEpsilonTarget",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/Transition;ZZZZ)Lorg/antlr/v4/runtime/atn/ATNConfig;"
            ) | (
                "getEpsilonTarget",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/Transition;ZZZZ)Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;"
            ) | (
                "computeReachSet",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;IZ)Lorg/antlr/v4/runtime/atn/ATNConfigSet;"
            ) | (
                "computeReachSet",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;IZ)Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;"
            ) | (
                "closureCheckingStopState",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V"
            ) | (
                "closureCheckingStopState",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V"
            ) | (
                "closure",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZZ)V"
            ) | (
                "closure",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZZ)V"
            ) | (
                "closure_",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V"
            ) | (
                "closure_",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V"
            )
        );
    }
    if class_name.ends_with("/atn/Transition") {
        return (method_name, descriptor) == ("isEpsilon", "()Z");
    }
    if matches!(
        class_name.rsplit('/').next().unwrap_or_default(),
        "EpsilonTransition"
            | "RangeTransition"
            | "RuleTransition"
            | "PredicateTransition"
            | "AtomTransition"
            | "ActionTransition"
            | "SetTransition"
            | "NotSetTransition"
            | "WildcardTransition"
            | "PrecedencePredicateTransition"
    ) && class_name.contains("/atn/")
    {
        return matches!(
            (method_name, descriptor),
            ("getSerializationType", "()I") | ("isEpsilon", "()Z") | ("matches", "(III)Z")
        );
    }
    if class_name.ends_with("/atn/PredictionMode") {
        return matches!(
            (method_name, descriptor),
            (
                "getConflictingAltSubsets",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;)Ljava/util/Collection;"
            ) | (
                "getConflictingAltSubsets",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;)Ljava/util/Collection;"
            ) | (
                "hasStateAssociatedWithOneAlt",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;)Z"
            ) | (
                "hasStateAssociatedWithOneAlt",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;)Z"
            )
        );
    }
    if class_name.ends_with("/atn/PredictionContext") {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I")
                | ("isEmpty", "()Z")
                | ("hasEmptyPath", "()Z")
                | ("calculateEmptyHashCode", "()I")
                | (
                    "calculateHashCode",
                    "(Lorg/antlr/v4/runtime/atn/PredictionContext;I)I"
                )
                | (
                    "calculateHashCode",
                    "([Lorg/antlr/v4/runtime/atn/PredictionContext;[I)I"
                )
                | (
                    "calculateHashCode",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;I)I"
                )
                | (
                    "calculateHashCode",
                    "([Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;[I)I"
                )
                | (
                    "merge",
                    "(Lorg/antlr/v4/runtime/atn/PredictionContext;Lorg/antlr/v4/runtime/atn/PredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeSingletons",
                    "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeRoot",
                    "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Z)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeArrays",
                    "(Lorg/antlr/v4/runtime/atn/ArrayPredictionContext;Lorg/antlr/v4/runtime/atn/ArrayPredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "merge",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeSingletons",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeRoot",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Z)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeArrays",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ArrayPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/ArrayPredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
        );
    }
    if class_name.ends_with("/atn/SingletonPredictionContext")
        || class_name.ends_with("/atn/EmptyPredictionContext")
        || class_name.ends_with("/atn/ArrayPredictionContext")
    {
        if method_name == "<init>" {
            return matches!(
                descriptor,
                "()V"
                    | "(Lorg/antlr/v4/runtime/atn/PredictionContext;I)V"
                    | "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;I)V"
                    | "([Lorg/antlr/v4/runtime/atn/PredictionContext;[I)V"
                    | "([Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;[I)V"
                    | "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;)V"
                    | "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;)V"
            );
        }
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I")
                | ("isEmpty", "()Z")
                | ("hasEmptyPath", "()Z")
                | ("size", "()I")
                | ("getReturnState", "(I)I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | (
                    "getParent",
                    "(I)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "getParent",
                    "(I)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
        );
    }
    false
}

pub(crate) fn is_bytebuddy_method_token_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if matches!(
        class_name,
        "net/bytebuddy/description/method/MethodDescription$TypeToken"
            | "net/bytebuddy/description/method/MethodDescription$SignatureToken"
            | "net/bytebuddy/dynamic/scaffold/MethodGraph$Compiler$Default$Harmonizer$ForJavaMethod$Token"
            | "net/bytebuddy/dynamic/scaffold/MethodGraph$Compiler$Default$Key"
    ) {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name == "net/bytebuddy/description/method/MethodDescription$TypeSubstituting" {
        return (method_name, descriptor)
            == (
                "<init>",
                "(Lnet/bytebuddy/description/type/TypeDescription$Generic;Lnet/bytebuddy/description/method/MethodDescription;Lnet/bytebuddy/description/type/TypeDescription$Generic$Visitor;)V",
            );
    }
    if matches!(
        class_name,
        "net/bytebuddy/description/method/MethodList$Explicit"
            | "net/bytebuddy/description/method/MethodList$TypeSubstituting"
            | "net/bytebuddy/description/method/MethodList$ForLoadedMethods"
            | "net/bytebuddy/description/method/MethodList$ForTokens"
            | "net/bytebuddy/description/field/FieldList$Explicit"
            | "net/bytebuddy/description/field/FieldList$ForTokens"
            | "net/bytebuddy/description/field/FieldList$ForLoadedFields"
            | "net/bytebuddy/description/type/TypeList$Explicit"
            | "net/bytebuddy/description/type/TypeList$Generic$Explicit"
    ) {
        return method_name == "size" && descriptor == "()I"
            || method_name == "get"
                && matches!(
                    descriptor,
                    "(I)Ljava/lang/Object;"
                        | "(I)Lnet/bytebuddy/description/method/MethodDescription;"
                        | "(I)Lnet/bytebuddy/description/method/MethodDescription$InGenericShape;"
                        | "(I)Lnet/bytebuddy/description/method/MethodDescription$InDefinedShape;"
                        | "(I)Lnet/bytebuddy/description/field/FieldDescription;"
                        | "(I)Lnet/bytebuddy/description/field/FieldDescription$InDefinedShape;"
                        | "(I)Lnet/bytebuddy/description/type/TypeDescription;"
                        | "(I)Lnet/bytebuddy/description/type/TypeDescription$Generic;"
                );
    }
    false
}

pub(crate) fn is_method_handles_varhandle_factory_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name == "java/lang/invoke/MethodHandles"
        && matches!(
            (method_name, method_descriptor),
            (
                "arrayElementVarHandle",
                "(Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"
            ) | (
                "byteArrayViewVarHandle",
                "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"
            ) | (
                "byteBufferViewVarHandle",
                "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"
            )
        )
}

pub(crate) fn is_mockito_debugging_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if class_name == "org/mockito/internal/creation/bytebuddy/MockMethodAdvice" {
        return method_name == "isOverridden"
            && descriptor == "(Ljava/lang/Object;Ljava/lang/reflect/Method;)Z";
    }
    if !matches!(
        class_name,
        "org/mockito/internal/debugging/LocationFactory"
            | "org/mockito/internal/debugging/LocationFactory$DefaultLocationFactory"
    ) {
        return false;
    }
    // Off by default: the real `LocationFactory` selector runs and picks the
    // StackWalker-backed `LocationImpl`, as on HotSpot. The legacy native
    // returned a `Java8LocationImpl` with a hardcoded
    // `"-> at <<unknown line>>"`, which erased the call site from every
    // Mockito diagnostic. See `flags::mockito_legacy_selectors`.
    if !cratonvm_types::flags::mockito_legacy_selectors() {
        return false;
    }
    method_name == "create"
        && matches!(
            descriptor,
            "()Lorg/mockito/invocation/Location;" | "(Z)Lorg/mockito/invocation/Location;"
        )
}

pub(crate) fn is_hibernate_testing_util_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/hibernate/testing/orm/junit/TestingUtil"
        && (method_name, descriptor)
            == (
                "hasEffectiveAnnotation",
                "(Lorg/junit/jupiter/api/extension/ExtensionContext;Ljava/lang/Class;)Z",
            )
}

pub(crate) fn is_hibernate_models_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if class_name == "org/hibernate/metamodel/mapping/AssociationKey" {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name == "org/hibernate/metamodel/mapping/internal/ImmutableAttributeMappingList" {
        return (method_name, descriptor)
            == (
                "indexedForEach",
                "(Lorg/hibernate/internal/util/IndexedConsumer;)V",
            );
    }
    if class_name == "org/hibernate/metamodel/mapping/BasicValuedModelPart" {
        return matches!(
            (method_name, descriptor),
            (
                "forEachSelectable",
                "(ILorg/hibernate/metamodel/mapping/SelectableConsumer;)I"
            ) | (
                "forEachSelectable",
                "(Lorg/hibernate/metamodel/mapping/SelectableConsumer;)I"
            )
        );
    }
    if class_name == "org/hibernate/models/internal/AnnotationUsageHelper" {
        return matches!(
            (method_name, descriptor),
            (
                "findUsage",
                "(Lorg/hibernate/models/spi/AnnotationDescriptor;Ljava/util/Map;)Ljava/lang/annotation/Annotation;"
            )
                | (
                    "getUsage",
                    "(Lorg/hibernate/models/spi/AnnotationDescriptor;Ljava/util/Map;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getUsage",
                    "(Ljava/lang/Class;Ljava/util/Map;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
                )
        );
    }
    if matches!(
        class_name,
        "org/hibernate/models/internal/AnnotationDescriptorRegistryStandard"
            | "org/hibernate/models/spi/AnnotationDescriptorRegistry"
    ) {
        return (method_name, descriptor)
            == (
                "getDescriptor",
                "(Ljava/lang/Class;)Lorg/hibernate/models/spi/AnnotationDescriptor;",
            );
    }
    matches!(
        class_name,
        "org/hibernate/models/internal/AnnotationTargetSupport"
            | "org/hibernate/models/spi/AnnotationTarget"
            | "org/hibernate/models/spi/MutableAnnotationTarget"
            | "org/hibernate/models/spi/AnnotationDescriptor"
            | "org/hibernate/models/spi/MutableAnnotationDescriptor"
            | "org/hibernate/models/spi/ClassDetails"
            | "org/hibernate/models/spi/MutableClassDetails"
            | "org/hibernate/models/spi/MemberDetails"
            | "org/hibernate/models/spi/MutableMemberDetails"
            | "org/hibernate/models/spi/FieldDetails"
            | "org/hibernate/models/spi/MethodDetails"
            | "org/hibernate/models/spi/RecordComponentDetails"
            | "org/hibernate/models/internal/AbstractAnnotationDescriptor"
            | "org/hibernate/models/internal/StandardAnnotationDescriptor"
            | "org/hibernate/models/internal/OrmAnnotationDescriptor"
    ) && matches!(
        (method_name, descriptor),
        ("hasDirectAnnotationUsage", "(Ljava/lang/Class;)Z")
            | (
                "getDirectAnnotationUsage",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
            )
            | (
                "hasAnnotationUsage",
                "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Z"
            )
            | (
                "getAnnotationUsage",
                "(Lorg/hibernate/models/spi/AnnotationDescriptor;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
            )
            | (
                "getAnnotationUsage",
                "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
            )
            | (
                "locateAnnotationUsage",
                "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
            )
    )
}

pub(crate) fn is_bitset_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/BitSet"
        && matches!(
            (method_name, descriptor),
            ("set", "(I)V")
                | ("set", "(IZ)V")
                | ("clear", "(I)V")
                | ("clear", "()V")
                | ("get", "(I)Z")
                | ("length", "()I")
                | ("cardinality", "()I")
                | ("isEmpty", "()Z")
                | ("nextSetBit", "(I)I")
                | ("nextClearBit", "(I)I")
        )
}

pub(crate) fn is_h2_parser_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // Hibernate's UUID monotonicity test executes AssertJ's successful
    // natural-order comparison path millions of times. The native preserves
    // custom-comparator and failure delegation, but must win over the real
    // inherited library bytecode to remove its per-assertion setup overhead.
    if class_name == "org/assertj/core/api/AbstractComparableAssert" {
        return (method_name, descriptor)
            == (
                "isGreaterThan",
                "(Ljava/lang/Comparable;)Lorg/assertj/core/api/AbstractComparableAssert;",
            );
    }
    if class_name == "org/assertj/core/api/AbstractStringAssert" {
        return (method_name, descriptor)
            == (
                "isGreaterThan",
                "(Ljava/lang/String;)Lorg/assertj/core/api/AbstractStringAssert;",
            );
    }
    if matches!(
        class_name,
        "org/assertj/core/api/AssertionsForClassTypes" | "org/assertj/core/api/Assertions"
    ) {
        return matches!(
            (method_name, descriptor),
            (
                "assertThat",
                "(Ljava/lang/String;)Lorg/assertj/core/api/AbstractStringAssert;"
            ) | (
                "assertThat",
                "(Ljava/lang/Comparable;)Lorg/assertj/core/api/AbstractComparableAssert;"
            )
        );
    }
    if matches!(
        class_name,
        "org/hibernate/id/uuid/UuidVersion6Strategy" | "org/hibernate/id/uuid/UuidVersion7Strategy"
    ) {
        return (method_name, descriptor)
            == (
                "generateUuid",
                "(Lorg/hibernate/engine/spi/SharedSessionContractImplementor;)Ljava/util/UUID;",
            );
    }
    if class_name == "org/h2/util/Utils" {
        return (method_name, descriptor) == ("getResource", "(Ljava/lang/String;)[B");
    }
    if class_name == "org/h2/constraint/ConstraintReferential" {
        return (method_name, descriptor)
            == ("checkExistingData", "(Lorg/h2/engine/SessionLocal;)V");
    }
    if class_name == "org/h2/mvstore/type/LongDataType" {
        return matches!(
            (method_name, descriptor),
            ("binarySearch", "(Ljava/lang/Long;Ljava/lang/Object;II)I")
                | ("binarySearch", "(Ljava/lang/Object;Ljava/lang/Object;II)I")
        );
    }
    if class_name == "org/h2/mvstore/RootReference" {
        return (method_name, descriptor)
            == (
                "updateRootPage",
                "(Lorg/h2/mvstore/Page;J)Lorg/h2/mvstore/RootReference;",
            );
    }
    if class_name == "org/h2/mvstore/tx/Transaction" {
        return (method_name, descriptor)
            == (
                "<init>",
                "(Lorg/h2/mvstore/tx/TransactionStore;IJILjava/lang/String;JIILorg/h2/engine/IsolationLevel;Lorg/h2/mvstore/tx/TransactionStore$RollbackListener;)V",
            );
    }
    if class_name == "org/h2/table/Column" {
        return matches!(
            (method_name, descriptor),
            ("equals", "(Ljava/lang/Object;)Z")
                | ("hashCode", "()I")
                | ("getTable", "()Lorg/h2/table/Table;")
        );
    }
    if class_name == "org/h2/engine/DbObject" {
        return matches!(
            (method_name, descriptor),
            ("equals", "(Ljava/lang/Object;)Z") | ("hashCode", "()I")
        );
    }
    if matches!(
        class_name,
        "org/h2/engine/Session" | "org/h2/engine/SessionLocal"
    ) {
        return (method_name, descriptor) == ("hashCode", "()I");
    }
    if class_name == "org/h2/command/ParserBase" {
        return matches!(
            (method_name, descriptor),
            ("read", "()V")
                | ("setTokenIndex", "(I)V")
                | ("readIf", "(I)Z")
                | ("addExpected", "(I)V")
                | ("testToken", "(Ljava/lang/String;Lorg/h2/command/Token;)Z")
        );
    }
    if class_name == "org/h2/command/Tokenizer" {
        return (method_name, descriptor) == ("eq", "(Ljava/lang/String;Ljava/lang/String;II)Z");
    }
    if class_name == "org/h2/expression/ExpressionVisitor" {
        return matches!(
            (method_name, descriptor),
            ("getType", "()I")
                | (
                    "getDependenciesVisitor",
                    "(Ljava/util/HashSet;)Lorg/h2/expression/ExpressionVisitor;",
                )
                | (
                    "getMaxModificationIdVisitor",
                    "()Lorg/h2/expression/ExpressionVisitor;",
                )
        );
    }
    if class_name == "org/h2/message/Trace" {
        return (method_name, descriptor) == ("isDebugEnabled", "()Z");
    }
    if class_name == "org/h2/message/TraceSystem" {
        return (method_name, descriptor) == ("isEnabled", "(I)Z");
    }
    // Hibernate's JSON-array unnest tests lower to two
    // `system_range(1, 1000)` joins. H2's Java `ValueBigint.get(long)` only
    // interns 0..99, causing the remaining immutable row values to be
    // repeatedly allocated in the nested scan. The registered native extends
    // that exact immutable cache through 1000; it must be admitted here for
    // real-JDK bytecode calls to reach it.
    if class_name == "org/h2/value/ValueBigint" {
        return (method_name, descriptor) == ("get", "(J)Lorg/h2/value/ValueBigint;");
    }
    if class_name == "org/h2/expression/condition/Comparison" {
        return matches!(
            (method_name, descriptor),
            (
                "compare",
                "(Lorg/h2/engine/SessionLocal;Lorg/h2/value/Value;Lorg/h2/value/Value;I)Lorg/h2/value/Value;",
            ) | (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            )
        );
    }
    if class_name == "org/h2/expression/ExpressionColumn" {
        return (method_name, descriptor)
            == (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            );
    }
    if class_name == "org/h2/value/Value" {
        return (method_name, descriptor) == ("isFalse", "()Z");
    }
    if class_name == "org/h2/expression/condition/ConditionAndOr" {
        return (method_name, descriptor)
            == (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            );
    }
    if class_name == "org/h2/expression/function/CoalesceFunction" {
        return (method_name, descriptor)
            == (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            );
    }
    if class_name == "org/h2/expression/function/CardinalityExpression" {
        return (method_name, descriptor)
            == (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            );
    }
    if class_name == "org/h2/index/RangeCursor" {
        return matches!(
            (method_name, descriptor),
            ("next", "()Z")
                | ("get", "()Lorg/h2/result/Row;")
                | ("getSearchRow", "()Lorg/h2/result/SearchRow;")
        );
    }
    if class_name == "org/h2/result/Row" {
        return (method_name, descriptor) == ("get", "([Lorg/h2/value/Value;I)Lorg/h2/result/Row;");
    }
    if class_name == "org/h2/result/DefaultRow" {
        return (method_name, descriptor) == ("getValue", "(I)Lorg/h2/value/Value;");
    }
    class_name.starts_with("org/h2/command/Token")
        && matches!(
            (method_name, descriptor),
            ("tokenType", "()I") | ("asIdentifier", "()Ljava/lang/String;") | ("isQuoted", "()Z")
        )
}

pub(crate) fn is_jdk_string_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/StringLatin1" && (method_name, descriptor) == ("inflate", "([BI[CII)V")
}

pub(crate) fn is_spring_mock_response_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "org/springframework/mock/web/MockHttpServletResponse"
            | "org/springframework/web/testfixture/servlet/MockHttpServletResponse"
    ) && method_name == "getContentAsString"
        && matches!(
            descriptor,
            "()Ljava/lang/String;" | "(Ljava/nio/charset/Charset;)Ljava/lang/String;"
        )
}

pub(crate) fn is_script_engine_manager_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "javax/script/ScriptEngineManager"
        && matches!(
            (method_name, descriptor),
            (
                "getEngineByName",
                "(Ljava/lang/String;)Ljavax/script/ScriptEngine;"
            ) | (
                "getEngineByExtension",
                "(Ljava/lang/String;)Ljavax/script/ScriptEngine;"
            ) | (
                "getEngineByMimeType",
                "(Ljava/lang/String;)Ljavax/script/ScriptEngine;"
            )
        )
}

pub(crate) fn is_jython_thread_state_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/python/core/Py"
        && matches!(
            (method_name, descriptor),
            ("importSiteIfSelected", "()Z")
                | ("getSystemState", "()Lorg/python/core/PySystemState;")
                | (
                    "setSystemState",
                    "(Lorg/python/core/PySystemState;)Lorg/python/core/PySystemState;",
                )
                | ("getThreadState", "()Lorg/python/core/ThreadState;")
                | (
                    "getThreadState",
                    "(Lorg/python/core/PySystemState;)Lorg/python/core/ThreadState;",
                )
        )
}

pub(crate) fn is_jython_pyobject_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/python/core/PyObject"
        && ((matches!(method_name, "_is" | "_isnot" | "_eq" | "_ne")
            && descriptor == "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;")
            || (method_name == "invoke"
                && descriptor
                    == "(Ljava/lang/String;Lorg/python/core/PyObject;)Lorg/python/core/PyObject;"))
}

pub(crate) fn is_jython_imp_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/python/core/imp"
        && method_name == "addModule"
        && descriptor == "(Ljava/lang/String;)Lorg/python/core/PyModule;"
}

pub(crate) fn is_jython_pymodule_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/python/core/PyModule"
        && method_name == "__findattr_ex__"
        && descriptor == "(Ljava/lang/String;)Lorg/python/core/PyObject;"
}

pub(crate) fn is_jdk_wrapper_math_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match class_name {
        "java/lang/Integer" => {
            descriptor == "(II)I" && matches!(method_name, "sum" | "max" | "min" | "compare")
        }
        "java/lang/Long" => {
            (descriptor == "(JJ)J" && matches!(method_name, "sum" | "max" | "min"))
                || (descriptor == "(JJ)I" && method_name == "compare")
        }
        _ => false,
    }
}

pub(super) fn is_bc_sect_field_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "org/bouncycastle/math/ec/custom/sec/SecT113Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT131Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT163Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT193Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT233Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT239Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT283Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT409Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT571Field"
    )
}

pub(super) fn is_bc_sect_field_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if !is_bc_sect_field_class(class_name) {
        return false;
    }
    matches!(
        (method_name, descriptor),
        (
            "add" | "addBothTo" | "addExt" | "multiply" | "multiplyAddToExt",
            "([J[J[J)V"
        ) | (
            "addOne" | "halfTrace" | "invert" | "reduce" | "sqrt" | "square" | "squareAddToExt",
            "([J[J)V"
        ) | ("squareN", "([JI[J)V")
            | ("trace", "([J)I")
    ) || (class_name == "org/bouncycastle/math/ec/custom/sec/SecT571Field"
        && matches!(
            (method_name, descriptor),
            ("precompMultiplicand", "([J)[J")
                | ("multiplyPrecomp" | "multiplyPrecompAddToExt", "([J[J[J)V")
        ))
}

pub(super) fn is_bc_sect_point_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "org/bouncycastle/math/ec/custom/sec/SecT113R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT113R2Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT131R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT131R2Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT163K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT163R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT163R2Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT193R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT193R2Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT233K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT233R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT239K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT283K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT283R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT409K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT409R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT571K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT571R1Point"
    )
}

pub(crate) fn is_bc_crypto_math_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if is_bc_sect_field_native_override(class_name, method_name, descriptor) {
        return true;
    }
    if class_name == "org/bouncycastle/math/ec/ECPoint"
        && method_name == "timesPow2"
        && descriptor == "(I)Lorg/bouncycastle/math/ec/ECPoint;"
    {
        return true;
    }
    if is_bc_sect_point_class(class_name)
        && method_name == "twice"
        && descriptor == "()Lorg/bouncycastle/math/ec/ECPoint;"
    {
        return true;
    }
    match class_name {
        "org/bouncycastle/math/ec/ECFieldElement$Fp" => matches!(
            (method_name, descriptor),
            (
                "add" | "subtract" | "multiply" | "divide",
                "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
            ) | ("addOne" | "square" | "negate" | "invert", "()Lorg/bouncycastle/math/ec/ECFieldElement;")
                | (
                    "modAdd" | "modMult" | "modSubtract",
                    "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                )
                | (
                    "modDouble" | "modHalf" | "modHalfAbs" | "modInverse" | "modReduce",
                    "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                )
                | (
                    "multiplyPlusProduct" | "multiplyMinusProduct",
                    "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
                )
                | (
                    "squarePlusProduct" | "squareMinusProduct",
                    "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
                )
        ),
        "org/bouncycastle/math/ec/ECFieldElement$F2m" => matches!(
            (method_name, descriptor),
            (
                "add" | "subtract" | "multiply" | "divide",
                "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
            ) | ("addOne" | "square" | "negate" | "invert", "()Lorg/bouncycastle/math/ec/ECFieldElement;")
                | (
                    "multiplyPlusProduct" | "multiplyMinusProduct",
                    "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
                )
                | (
                    "squarePlusProduct" | "squareMinusProduct",
                    "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
                )
                | ("squarePow", "(I)Lorg/bouncycastle/math/ec/ECFieldElement;")
        ),
        "org/bouncycastle/math/ec/ECPoint$F2m" => matches!(
            (method_name, descriptor),
            (
                "add" | "twicePlus",
                "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;"
            ) | ("twice", "()Lorg/bouncycastle/math/ec/ECPoint;")
        ),
        "org/bouncycastle/math/ec/ECPoint$Fp" => matches!(
            (method_name, descriptor),
            (
                "add" | "twicePlus",
                "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;"
            ) | (
                "twice" | "threeTimes" | "negate",
                "()Lorg/bouncycastle/math/ec/ECPoint;"
            ) | ("timesPow2", "(I)Lorg/bouncycastle/math/ec/ECPoint;")
        ),
        "org/bouncycastle/math/ec/ECAlgorithms" => matches!(
            (method_name, descriptor),
            (
                "implShamirsTrickJsf",
                "(Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;)Lorg/bouncycastle/math/ec/ECPoint;"
            )
        ),
        "org/bouncycastle/math/ec/LongArray" => matches!(
            (method_name, descriptor),
            ("modReduce" | "modSquare" | "modInverse", "(I[I)Lorg/bouncycastle/math/ec/LongArray;")
                | ("reduce", "(I[I)V")
                | (
                    "modMultiply" | "multiply",
                    "(Lorg/bouncycastle/math/ec/LongArray;I[I)Lorg/bouncycastle/math/ec/LongArray;"
                )
                | ("square", "(I[I)Lorg/bouncycastle/math/ec/LongArray;")
                | ("modSquareN", "(II[I)Lorg/bouncycastle/math/ec/LongArray;")
        ),
        "org/bouncycastle/math/Primes" => matches!(
            (method_name, descriptor),
            ("implHasAnySmallFactors", "(Ljava/math/BigInteger;)Z")
                | (
                    "isMRProbablePrime",
                    "(Ljava/math/BigInteger;Ljava/security/SecureRandom;I)Z"
                )
                | (
                    "isMRProbablePrimeToBase",
                    "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Z"
                )
        ),
        "org/bouncycastle/math/ec/rfc7748/X25519Field" => {
            method_name == "mul" && descriptor == "([I[I[I)V"
        }
        "org/bouncycastle/math/ec/rfc7748/X448Field" => matches!(
            (method_name, descriptor),
            ("mul", "([I[I[I)V")
                | ("mul", "([II[I)V")
                | ("sqr", "([I[I)V")
                | ("sqr", "([II[I)V")
        ),
        "org/bouncycastle/util/BigIntegers" => matches!(
            (method_name, descriptor),
            ("hasAnySmallFactors", "(Ljava/math/BigInteger;)Z")
                | (
                    "modOddInverse" | "modOddInverseVar",
                    "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                )
        ),
        "org/bouncycastle/crypto/prng/DigestRandomGenerator" => matches!(
            (method_name, descriptor),
            ("nextBytes", "([B)V") | ("nextBytes", "([BII)V")
        ),
        "org/bouncycastle/crypto/engines/GOST3412_2015Engine" => {
            method_name == "processBlock" && descriptor == "([BI[BI)I"
        }
        "org/bouncycastle/crypto/engines/SM4Engine" => {
            method_name == "processBlock" && descriptor == "([BI[BI)I"
        }
        "org/bouncycastle/crypto/engines/XTEAEngine" => {
            method_name == "processBlock" && descriptor == "([BI[BI)I"
        }
        "org/bouncycastle/crypto/engines/Salsa20Engine" => {
            (method_name == "salsaCore" && descriptor == "(I[I[I)V")
                || (method_name == "processBytes" && descriptor == "([BII[BI)I")
        }
        "org/bouncycastle/crypto/engines/XSalsa20Engine"
        | "org/bouncycastle/crypto/engines/ChaChaEngine"
        | "org/bouncycastle/crypto/engines/ChaCha7539Engine"
        | "org/bouncycastle/crypto/engines/XChaCha20Engine" => {
            method_name == "processBytes" && descriptor == "([BII[BI)I"
        }
        "org/bouncycastle/crypto/engines/VMPCEngine"
        | "org/bouncycastle/crypto/engines/VMPCKSA3Engine" => {
            method_name == "processBytes" && descriptor == "([BII[BI)I"
        }
        "org/bouncycastle/crypto/engines/AESEngine" => matches!(
            (method_name, descriptor),
            ("encryptBlock" | "decryptBlock", "([BI[BI[[I)V")
                | ("<init>", "()V")
                | (
                    "newInstance",
                    "()Lorg/bouncycastle/crypto/MultiBlockCipher;"
                )
                | ("generateWorkingKey", "([BZ)[[I")
                | (
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V"
                )
                | ("processBlock", "([BI[BI)I")
        ),
        "org/bouncycastle/crypto/engines/AESLightEngine"
        | "org/bouncycastle/crypto/engines/AESFastEngine" => matches!(
            (method_name, descriptor),
            ("encryptBlock" | "decryptBlock", "([BI[BI[[I)V")
                | ("<init>", "()V")
                | ("generateWorkingKey", "([BZ)[[I")
                | (
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V"
                )
                | ("processBlock", "([BI[BI)I")
        ),
        "org/bouncycastle/crypto/modes/SICBlockCipher" => matches!(
            (method_name, descriptor),
            ("reset", "()V")
                | ("seekTo", "(J)J")
                | (
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V"
                )
                | ("processBlock", "([BI[BI)I")
                | ("processBytes", "([BII[BI)I")
        ),
        "org/bouncycastle/crypto/modes/CBCBlockCipher" => {
            method_name == "processBlock" && descriptor == "([BI[BI)I"
        }
        "org/bouncycastle/util/Pack" => matches!(
            (method_name, descriptor),
            ("bigEndianToInt" | "littleEndianToInt", "([BI)I")
                | ("bigEndianToInt" | "littleEndianToInt", "([BI[I)V")
                | ("bigEndianToInt" | "littleEndianToInt", "([BI[III)V")
                | ("intToBigEndian" | "intToLittleEndian", "(I[BI)V")
                | ("intToBigEndian" | "intToLittleEndian", "([I[BI)V")
                | ("intToBigEndian" | "intToLittleEndian", "([III[BI)V")
        ),
        "org/bouncycastle/util/Arrays" => method_name == "copyOf" && descriptor == "([BI)[B",
        "org/bouncycastle/crypto/params/KeyParameter" => matches!(
            (method_name, descriptor),
            ("<init>", "([B)V") | ("<init>", "([BII)V")
        ),
        "org/bouncycastle/crypto/params/ParametersWithIV" => matches!(
            (method_name, descriptor),
            (
                "<init>",
                "(Lorg/bouncycastle/crypto/CipherParameters;[B)V"
            ) | (
                "<init>",
                "(Lorg/bouncycastle/crypto/CipherParameters;[BII)V"
            )
        ),
        "org/bouncycastle/crypto/digests/Blake2sDigest" => {
            (method_name == "G" && descriptor == "(IIIIII)V")
                || (method_name == "compress" && descriptor == "([BI)V")
        }
        // LMS/HSS hashes through this class directly (its `DigestUtil` builds
        // `new SHA256Digest()`), never through `MessageDigest`, so the native
        // JCA SHA-256 is on a path that workload cannot reach. `processBlock`
        // is the compression leaf; everything around it stays real bytecode.
        // `SHA256Digest` has no BouncyCastle subclass, so the superclass walk
        // in `intercept_force_registered_native` cannot divert another digest's
        // `processBlock` here.
        "org/bouncycastle/crypto/digests/SHA256Digest" => {
            method_name == "processBlock" && descriptor == "()V"
        }
        "org/bouncycastle/crypto/digests/KeccakDigest" => matches!(
            (method_name, descriptor),
            ("KeccakPermutation" | "KeccakExtract", "()V") | ("KeccakAbsorb", "([BI)V")
        ),
        "org/bouncycastle/crypto/digests/GOST3411Digest" => {
            method_name == "processBlock" && descriptor == "([BI)V"
        }
        "org/bouncycastle/crypto/digests/WhirlpoolDigest" => matches!(
            (method_name, descriptor),
            ("processBlock", "()V") | ("update", "([BII)V")
        ),
        "org/bouncycastle/crypto/macs/Poly1305" => matches!(
            (method_name, descriptor),
            ("update", "([BII)V") | ("doFinal", "([BI)I")
        ),
        "org/bouncycastle/crypto/generators/SCrypt" => {
            method_name == "generate" && descriptor == "([B[BIIII)[B"
        }
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator" => matches!(
            (method_name, descriptor),
            ("generateBytes", "([B[BII)I")
                | (
                    "roundFunction",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;IIIIIIIIIIIIIIII)V"
                )
        ),
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$Block" => matches!(
            (method_name, descriptor),
            ("fromBytes" | "toBytes", "([B)V")
                | (
                    "copyBlock" | "xorWith",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
                )
                | (
                    "xor" | "xorWith",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
                )
                | (
                    "clear",
                    "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;"
                )
        ),
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FillBlock" => matches!(
            (method_name, descriptor),
            ("applyBlake", "()V")
                | (
                    "fillBlock",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
                )
                | (
                    "fillBlock" | "fillBlockWithXor",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
                )
        ),
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FixedBlockPool" => matches!(
            (method_name, descriptor),
            (
                "allocate",
                "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;"
            ) | (
                "deallocate",
                "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
            )
        ),
        "org/bouncycastle/crypto/generators/PKCS5S2ParametersGenerator" => matches!(
            (method_name, descriptor),
            (
                "generateDerivedParameters" | "generateDerivedMacParameters",
                "(I)Lorg/bouncycastle/crypto/CipherParameters;"
            ) | (
                "generateDerivedParameters",
                "(II)Lorg/bouncycastle/crypto/CipherParameters;"
            )
        ),
        "org/bouncycastle/crypto/generators/PKCS12ParametersGenerator" => matches!(
            (method_name, descriptor),
            (
                "generateDerivedParameters" | "generateDerivedMacParameters",
                "(I)Lorg/bouncycastle/crypto/CipherParameters;"
            ) | (
                "generateDerivedParameters",
                "(II)Lorg/bouncycastle/crypto/CipherParameters;"
            )
        ),
        "org/bouncycastle/crypto/generators/BCrypt" => {
            method_name == "generate" && descriptor == "([B[BI)[B"
        }
        _ => false,
    }
}

pub(crate) fn is_forkjoin_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // On the default real-ForkJoinPool path, keep real pool initialization but
    // force the VM bridge methods that otherwise enqueue work into
    // ForkJoinPool's queue/status machinery. The task-family methods share the
    // side-table state scanned and remapped by GC. The legacy synthetic path is
    // selected explicitly with CRATONVM_SYNTHETIC_FORKJOINPOOL.
    if class_name == "java/util/concurrent/ForkJoinPool"
        && matches!(
            (method_name, descriptor),
            ("commonPool", "()Ljava/util/concurrent/ForkJoinPool;")
                | (
                    "getFactory",
                    "()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;"
                )
                | ("getParallelism", "()I")
                | ("getCommonPoolParallelism", "()I")
                | (
                    "invoke",
                    "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;"
                )
                | (
                    "submit",
                    "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;"
                )
                | (
                    "externalSubmit",
                    "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;"
                )
                // submit(Callable)/submit(Runnable)/submit(Runnable, T): left off
                // the original allow-list, so real bytecode ran them against a pool
                // whose commonPool() shortcut never populates queues/runState/mode —
                // RejectedExecutionException at submissionQueue() (RealFjp.java).
                | (
                    "submit",
                    "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/ForkJoinTask;"
                )
                | (
                    "submit",
                    "(Ljava/lang/Runnable;)Ljava/util/concurrent/ForkJoinTask;"
                )
                | (
                    "submit",
                    "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/ForkJoinTask;"
                )
                | ("execute", "(Ljava/lang/Runnable;)V")
                | ("execute", "(Ljava/util/concurrent/ForkJoinTask;)V")
                | (
                    "awaitQuiescence",
                    "(JLjava/util/concurrent/TimeUnit;)Z"
                )
                // BULK SUBMISSION — `invokeAll(Collection)` is the overload
                // Weld's ConcurrentBeanDeployer ->
                // AbstractExecutorServices.invokeAllAndCheckForExceptions
                // calls. It was on NEITHER allow-list, so it fell through to
                // real JDK bytecode against the under-initialized
                // `commonPool()` bridge object and threw
                // RejectedExecutionException at submissionQueue() — the whole
                // `org.hibernate.orm.test.cdi.*` cluster. `invokeAny` was
                // uncovered too and failed silently (ran the callables, then
                // returned null), and `lazySubmit` threw like invokeAll.
                | ("invokeAll", "(Ljava/util/Collection;)Ljava/util/List;")
                | (
                    "invokeAll",
                    "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/util/List;"
                )
                | (
                    "invokeAllUninterruptibly",
                    "(Ljava/util/Collection;)Ljava/util/List;"
                )
                | ("invokeAny", "(Ljava/util/Collection;)Ljava/lang/Object;")
                | (
                    "invokeAny",
                    "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"
                )
                | (
                    "lazySubmit",
                    "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;"
                )
        )
    {
        return true;
    }

    matches!(
        class_name,
        "java/util/concurrent/ForkJoinTask"
            | "java/util/concurrent/RecursiveTask"
            | "java/util/concurrent/RecursiveAction"
    ) && matches!(
        (method_name, descriptor),
        ("fork", "()Ljava/util/concurrent/ForkJoinTask;")
            | ("join", "()Ljava/lang/Object;")
            | ("invoke", "()Ljava/lang/Object;")
            | ("get", "()Ljava/lang/Object;")
            | (
                "get",
                "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"
            )
            | ("getRawResult", "()Ljava/lang/Object;")
            | ("setRawResult", "(Ljava/lang/Object;)V")
            | ("isDone", "()Z")
            | ("isCompletedNormally", "()Z")
            // Reads the same side-table done/cancelled/thrown bits as
            // isCompletedNormally and getException, so the three cannot
            // disagree. Must stay in step with
            // `keep_real_forkjointask_bridge` in native-api/src/registry.rs.
            | ("isCompletedAbnormally", "()Z")
            | ("isCancelled", "()Z")
            | ("cancel", "(Z)Z")
            | ("complete", "(Ljava/lang/Object;)V")
            // Reads the throwable the side table records for an abnormally
            // completed task, so it cannot disagree with join()/get() about
            // whether the task failed.
            | ("getException", "()Ljava/lang/Throwable;")
            // W6-9: the WRITER for that record. Registered by
            // `native-builtins/src/phases_early.rs`. Must stay in step with
            // `keep_real_forkjointask_bridge` in native-api/src/registry.rs.
            | ("completeExceptionally", "(Ljava/lang/Throwable;)V")
            // W6-9 §7.2: registered by
            // `native-builtins/src/phases_late/concurrent.rs::
            // register_forkjointask_w6_9_residual_bridge`. Must stay in step
            // with `keep_real_forkjointask_bridge` in native-api/src/registry.rs.
            | ("reinitialize", "()V")
            // L12: the STATIC `invokeAll` overloads — the last real-bytecode
            // route from the lazy `fork()` above to an `awaitDone()` that no
            // worker thread can satisfy. Must stay in step with
            // `keep_real_forkjointask_bridge` in native-api/src/registry.rs.
            | (
                "invokeAll",
                "(Ljava/util/concurrent/ForkJoinTask;Ljava/util/concurrent/ForkJoinTask;)V"
            )
            | ("invokeAll", "([Ljava/util/concurrent/ForkJoinTask;)V")
            | ("invokeAll", "(Ljava/util/Collection;)Ljava/util/Collection;")
            // W6-7: the `quietly*` family — the remaining real-bytecode routes
            // into `doExec()` + `awaitDone()`, which no worker thread exists to
            // satisfy. Registered by
            // `native-builtins/src/phases_late/concurrent.rs::
            // register_forkjointask_quietly_bridge`. Must stay in step with
            // `keep_real_forkjointask_bridge` in native-api/src/registry.rs.
            | ("quietlyJoin", "()V")
            | ("quietlyInvoke", "()V")
            | ("quietlyComplete", "()V")
            | ("quietlyJoin", "(JLjava/util/concurrent/TimeUnit;)Z")
            | ("quietlyJoinUninterruptibly", "(JLjava/util/concurrent/TimeUnit;)Z")
            | ("quietlyJoinPoolInvokeAllTask", "(J)V")
            // The two STATIC accessors backed by `FJP_POOL_STACK`. Must stay
            // in step with `keep_real_forkjointask_bridge` in
            // native-api/src/registry.rs — that list decides whether the
            // registration SURVIVES, this one decides whether it WINS, and a
            // registration on one list only is inert in exactly the way that
            // reads as a fixed bug.
            | ("inForkJoinPool", "()Z")
            | ("getPool", "()Ljava/util/concurrent/ForkJoinPool;")
    )
}

pub(crate) fn is_count_down_latch_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/concurrent/CountDownLatch"
        && matches!(
            (method_name, descriptor),
            ("<init>", "(I)V")
                | ("countDown", "()V")
                | ("await", "()V")
                | ("await", "(JLjava/util/concurrent/TimeUnit;)Z")
                | ("getCount", "()J")
                | ("toString", "()Ljava/lang/String;")
        )
}

pub(crate) fn is_ffm_symbol_lookup_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/foreign/SymbolLookup"
        && method_name == "find"
        && descriptor == "(Ljava/lang/String;)Ljava/util/Optional;"
}

pub(crate) fn is_ffm_group_layout_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    (class_name == "java/lang/foreign/GroupLayout"
        || class_name == "java/lang/foreign/StructLayout")
        && matches!(
            (method_name, descriptor),
            ("memberLayouts", "()Ljava/util/List;")
                | ("name", "()Ljava/util/Optional;")
                | (
                    "withName",
                    "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;"
                )
                | ("byteSize", "()J")
                | ("byteAlignment", "()J")
        )
}

/// FFM `java.lang.foreign.MemoryLayout` — the layout factories and the two
/// instance methods `foreign_ffm.rs` answers on the interface itself.
///
/// **Descriptors javac cannot emit are not listed** (F27, 2026-08-13). Four
/// entries spelling the four factories with a `…)Ljava/lang/foreign/MemoryLayout;`
/// return were deleted here and at the inline copy of this table further down
/// `force_native_over_real_jdk_bytecode`. `javap java.lang.foreign.MemoryLayout`
/// on 25.0.3+9-LTS:
///
/// ```text
///   public static java.lang.foreign.PaddingLayout  paddingLayout(long);
///   public static java.lang.foreign.SequenceLayout sequenceLayout(long, MemoryLayout);
///   public static java.lang.foreign.StructLayout   structLayout(MemoryLayout...);
///   public static java.lang.foreign.UnionLayout    unionLayout(MemoryLayout...);
/// ```
///
/// A call site's descriptor comes from the resolved method's own descriptor, so
/// no classfile can name the erased-return spellings, and — measured — no
/// registration anywhere in the workspace answers them either: they were
/// `panama.rs`'s, and F16 deleted those rows on 2026-08-13. Their only remaining
/// occurrences in the tree were the two copies of this table.
///
/// Removing a force-route entry IS a behaviour change: it decides whether a
/// registered native shadows real JDK bytecode. Here it cannot be, because the
/// triple has no call site AND no registration — the predicate could only ever
/// have cost a fruitless registry probe for a call that cannot occur. The
/// JDK-true spellings beside them are untouched and still registered
/// (`foreign_ffm.rs::structLayout`/`sequenceLayout`/`unionLayout`/
/// `paddingLayout`), which is what keeps the bootstrap-cycle escape the inline
/// copy's comment describes.
///
/// `withName(String)Ljava/lang/foreign/MemoryLayout;` STAYS: `javap` shows
/// `public abstract java.lang.foreign.MemoryLayout withName(java.lang.String);`,
/// so that one is the real descriptor, not an erased twin.
pub(crate) fn is_ffm_memory_layout_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/foreign/MemoryLayout"
        && matches!(
            (method_name, descriptor),
            (
                "sequenceLayout",
                "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;"
            ) | (
                "structLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;"
            ) | (
                "unionLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;"
            ) | ("paddingLayout", "(J)Ljava/lang/foreign/PaddingLayout;")
                | (
                    "varHandle",
                    "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;"
                )
                | ("name", "()Ljava/util/Optional;")
                | (
                    "withName",
                    "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;"
                )
        )
}

/// FFM `java.lang.foreign.Arena` — the interface-native exemption that pairs
/// with `force_ffm_memory_segment_interface_native` (vm/src/vm/vm_exec.rs).
///
/// `Arena.ofConfined()/ofAuto()/ofShared()/global()` are STATIC interface
/// methods, so their registered natives always run (the interface skip only
/// covers instance methods) and hand back a synthetic receiver stamped with
/// the literal interface class name `java/lang/foreign/Arena`
/// (native-builtins/src/phases_late/foreign_ffm.rs `p67_new_arena`, which also
/// gives the arena the session that `scope()` hands out and `close()` closes).
/// Every lifecycle method on that receiver is a NON-STATIC method whose
/// declaring class is an interface — exactly the shape both dispatch guards
/// skip — so `scope()`/`close()`/`allocate(…)` would resolve to the interface's
/// own declaration (abstract in a real JDK image, a stub body under
/// `--synthetic-jdk`) instead of the natives that own the arena's lifetime.
/// `MemorySegment.scope` is already force-routed for precisely this reason;
/// without this twin the two disagree and `arena.scope() != segment.scope()`.
///
/// Only triples that actually have a registration are listed: `scope`, `close`,
/// the four `allocate` overloads and `allocateFrom`/`allocateUtf8String`
/// (foreign_ffm.rs `register_p67_foreign_memory` + panama.rs
/// `register_pe_arena`/`register_pe2_string_marshaling`). `allocateArray` has
/// no native behind it on any class and is deliberately absent — forcing a name
/// with no registration would only cost a fruitless registry probe.
///
/// A REAL `jdk.internal.foreign.ArenaImpl` receiver is unaffected: it declares
/// `scope()`, `close()` and `allocate(long, long)` concretely, so dispatch
/// resolves with `jdk/internal/foreign/ArenaImpl` as the declaring class and
/// never matches this predicate. (The session natives these bodies call do have
/// a real-receiver escape — `p67_session_delegate`'s
/// `invoke_virtual_bytecode_only` — but this predicate never needs it, because
/// a real `ArenaImpl` is not routed here in the first place.)
pub(crate) fn is_ffm_arena_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/foreign/Arena"
        && matches!(
            (method_name, descriptor),
            ("scope", "()Ljava/lang/foreign/MemorySegment$Scope;")
                | ("close", "()V")
                | ("allocate", "(J)Ljava/lang/foreign/MemorySegment;")
                | ("allocate", "(JJ)Ljava/lang/foreign/MemorySegment;")
                | (
                    "allocate",
                    "(Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/MemorySegment;"
                )
                | (
                    "allocate",
                    "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemorySegment;"
                )
                | (
                    "allocateFrom",
                    "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;"
                )
                | (
                    "allocateUtf8String",
                    "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;"
                )
        )
}

pub(crate) fn is_file_channel_impl_open_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "sun/nio/ch/FileChannelImpl"
        && method_name == "open"
        && matches!(
            descriptor,
            "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;"
                | "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/lang/Object;)Ljava/nio/channels/FileChannel;"
        )
}

/// `FileSystemProvider`'s three link operations —
/// `createSymbolicLink`/`createLink`/`readSymbolicLink`.
///
/// Same shape as the `newFileChannel` exemption further down this file: the
/// real-JDK base class gives each of these a CONCRETE body that unconditionally
/// throws `UnsupportedOperationException` (a concrete subclass is expected to
/// override it). CratonVM's default provider is the synthetic instance stamped
/// as the literal `java/nio/file/spi/FileSystemProvider` class, so there is no
/// subclass to override anything and "real class bytes are authoritative" runs
/// the throw. Without this exemption the natives registered in
/// `native-builtins/src/phases_late/nio_file.rs` are unreachable and every
/// `Files.createSymbolicLink` in the VM dies with a bare
/// `UnsupportedOperationException` — see
/// `files-createsymboliclink-unsupported-FIXED.md`.
///
/// The real `sun.nio.fs.*` provider names are listed alongside the base for the
/// same reason `newFileChannel` lists them: a cached dispatch site can carry a
/// concrete receiver class name while the callback lives under the base name.
///
/// This is the single source of truth for the exemption: it is consulted by
/// BOTH dispatch gates — [`force_native_over_real_jdk_bytecode`] and the
/// `check_override` predicate in `vm_exec.rs::invoke_on_class_shared_inner`.
pub(crate) fn is_file_system_provider_link_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "java/nio/file/spi/FileSystemProvider"
            | "sun/nio/fs/WindowsFileSystemProvider"
            | "sun/nio/fs/UnixFileSystemProvider"
    ) && matches!(
        (method_name, descriptor),
        (
            "createSymbolicLink",
            "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V"
        ) | ("createLink", "(Ljava/nio/file/Path;Ljava/nio/file/Path;)V")
            | (
                "readSymbolicLink",
                "(Ljava/nio/file/Path;)Ljava/nio/file/Path;"
            )
    )
}

pub(crate) fn is_input_stream_transfer_to_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // Kept in step with `register_p59_bulk_stream_transfer`, which no longer
    // registers on `java/io/InputStream`: an override of the ROOT class covers
    // every stream in the process, and a method served from Rust cannot carry
    // an agent's woven advice. See that registrar for the measurement.
    matches!(
        class_name,
        "java/io/ByteArrayInputStream" | "java/io/FileInputStream"
    ) && method_name == "transferTo"
        && descriptor == "(Ljava/io/OutputStream;)J"
}

pub(crate) fn is_zip_output_primitive_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    (class_name == "java/util/zip/ZipOutputStream"
        && matches!(
            (method_name, descriptor),
            ("writeShort", "(I)V")
                | ("writeInt", "(J)V")
                | ("writeLong", "(J)V")
                | ("putNextEntry", "(Ljava/util/zip/ZipEntry;)V")
                | ("write", "([BII)V")
                | ("write", "([B)V")
                | ("write", "(I)V")
                | ("finish", "()V")
                | ("close", "()V")
        ))
        || (class_name == "java/util/zip/ZipEntry"
            && method_name == "setTime"
            && descriptor == "(J)V")
        // The real InflaterInputStream.close bytecode is contractually simple,
        // but it runs once for every ZIP64 entry in the loader fixture. The
        // registered native performs the same closed/inflater/underlying-stream
        // transitions through real named fields; force it so virtual dispatch
        // does not fall back to the interpreter for each tiny entry.
        || (class_name == "java/util/zip/InflaterInputStream"
            && matches!(
                (method_name, descriptor),
                ("close", "()V")
                    | ("<init>", "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V")
                    | ("read", "()I")
                    | ("read", "([BII)I")
            ))
        || (class_name == "org/springframework/boot/loader/jar/ZipInflaterInputStream"
            && matches!(
                (method_name, descriptor),
                ("read", "([BII)I")
                    | ("<init>", "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V")
            ))
        || (class_name == "org/springframework/boot/loader/zip/FileDataBlock"
            && matches!(
                (method_name, descriptor),
                ("open", "()V")
                    | ("close", "()V")
                    | ("read", "(Ljava/nio/ByteBuffer;J)I")
            ))
}

pub(crate) fn is_native_thread_set_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "sun/nio/ch/NativeThreadSet"
        && matches!(
            (method_name, descriptor),
            ("add", "()I") | ("remove", "(I)V") | ("signalAndWait", "()V")
        )
}

/// JavaNioAccess methods that must dispatch through CratonVM natives even when
/// the real JDK returns an anonymous/synthetic access singleton. JDK 17's
/// `VM$BufferPoolsHolder.<clinit>` invokes this through the interface, and a
/// receiver-class lookup alone can miss the bridge native.
pub(crate) fn is_java_nio_access_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "jdk/internal/access/JavaNioAccess"
        && method_name == "getDirectBufferPool"
        && descriptor == "()Ljdk/internal/misc/VM$BufferPool;"
}

/// The three COUNTERS on `java.nio.Bits`' anonymous `VM$BufferPool`.
///
/// `Bits.RESERVED_MEMORY` / `TOTAL_CAPACITY` / `COUNT` are maintained by
/// `Bits.reserveMemory`, which this VM never reaches: `ByteBuffer
/// .allocateDirect` is force-overridden above, in every mode, by an allocator
/// that keeps its own counters (`native-io`'s `direct_buffer::bits()`). The
/// JDK's three therefore read zero forever, and the platform MBean server
/// publishes exactly these objects — so `java.nio:type=BufferPool,name=direct`
/// reported a perfect cache no matter what the VM was doing. MEASURED
/// 2026-09-01, `probes/PoolRoutes.java`, both modes, both binaries.
///
/// `getName()` is deliberately NOT here: it is two instructions returning
/// `"direct"` and the JDK's own answer is right.
pub(crate) fn is_direct_buffer_pool_counter_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/nio/Bits$1"
        && matches!(
            (method_name, descriptor),
            ("getCount", "()J") | ("getMemoryUsed", "()J") | ("getTotalCapacity", "()J")
        )
}

pub(crate) fn is_stamped_lock_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/concurrent/locks/StampedLock"
        && matches!(
            (method_name, descriptor),
            ("<init>", "()V")
                | ("readLock", "()J")
                | ("writeLock", "()J")
                | ("tryOptimisticRead", "()J")
                | ("unlockRead", "(J)V")
                | ("unlockWrite", "(J)V")
                | ("unstampedUnlockRead", "()V")
                | ("unstampedUnlockWrite", "()V")
                | ("tryUnlockRead", "()Z")
                | ("tryUnlockWrite", "()Z")
                | ("validate", "(J)Z")
                | ("tryReadLock", "()J")
                | ("tryWriteLock", "()J")
                | ("tryConvertToWriteLock", "(J)J")
                | ("tryConvertToReadLock", "(J)J")
                | ("tryConvertToOptimisticRead", "(J)J")
                | ("unlock", "(J)V")
                | ("readLockInterruptibly", "()J")
                | ("writeLockInterruptibly", "()J")
                | ("tryReadLock", "(JLjava/util/concurrent/TimeUnit;)J")
                | ("tryWriteLock", "(JLjava/util/concurrent/TimeUnit;)J")
                | ("isLocked", "()Z")
                | ("isWriteLocked", "()Z")
                | ("isReadLocked", "()Z")
                | ("getReadLockCount", "()I")
        )
        || matches!(
            class_name,
            "java/util/concurrent/locks/StampedLock$ReadLockView"
                | "java/util/concurrent/locks/StampedLock$WriteLockView"
        ) && matches!(
            (method_name, descriptor),
            ("lock", "()V") | ("tryLock", "()Z") | ("unlock", "()V")
        )
}

/// The real JDK 25 ReentrantReadWriteLock stores its state in the final
/// protected helpers inherited from AbstractQueuedLongSynchronizer.  The
/// native implementations retain the JDK queue algorithm while providing an
/// atomic scalar state word outside CratonVM's tagged heap slot.
pub(crate) fn is_aqls_state_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/concurrent/locks/AbstractQueuedLongSynchronizer"
        && matches!(
            (method_name, descriptor),
            ("getState", "()J") | ("setState", "(J)V") | ("compareAndSetState", "(JJ)Z")
        )
}

pub(crate) fn is_xerces_cmstateset_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "com/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet"
        && matches!(
            (method_name, descriptor),
            ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | (
                    "isSameSet",
                    "(Lcom/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet;)Z"
                )
        )
}

pub(crate) fn is_xerces_xml_parser_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match class_name {
        "com/sun/org/apache/xerces/internal/util/XMLChar" => matches!(
            (method_name, descriptor),
            ("isSpace", "(I)Z")
                | ("isNameStart", "(I)Z")
                | ("isName", "(I)Z")
                | ("isNCNameStart", "(I)Z")
                | ("isNCName", "(I)Z")
        ),
        "jdk/xml/internal/XMLLimitAnalyzer" => matches!(
            (method_name, descriptor),
            ("addValue", "(ILjava/lang/String;I)V")
                | (
                    "addValue",
                    "(Ljdk/xml/internal/XMLSecurityManager$Limit;Ljava/lang/String;I)V"
                )
                | ("getValue", "(I)I")
                | ("getValue", "(Ljdk/xml/internal/XMLSecurityManager$Limit;)I")
                | ("getTotalValue", "(I)I")
                | (
                    "getTotalValue",
                    "(Ljdk/xml/internal/XMLSecurityManager$Limit;)I"
                )
                | ("getValueByIndex", "(I)I")
        ),
        "com/sun/org/apache/xerces/internal/impl/dv/xs/XSSimpleTypeDecl" => matches!(
            (method_name, descriptor),
            ("normalize", "(Ljava/lang/String;S)Ljava/lang/String;")
                | ("normalize", "(Ljava/lang/Object;S)Ljava/lang/String;")
        ),
        "com/sun/org/apache/xerces/internal/impl/xs/traversers/XSDHandler$XSDKey" => matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        ),
        "com/sun/org/apache/xerces/internal/impl/XMLEntityScanner" => matches!(
            (method_name, descriptor),
            (
                "scanContent",
                "(Lcom/sun/org/apache/xerces/internal/xni/XMLString;)I"
            ) | (
                "scanQName",
                "(Lcom/sun/org/apache/xerces/internal/xni/QName;Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z"
            ) | ("skipSpaces", "()Z") | (
                "normalizeNewlines",
                "(SLcom/sun/org/apache/xerces/internal/xni/XMLString;ZZLcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z"
            ) | (
                "checkEntityLimit",
                "(Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;Lcom/sun/xml/internal/stream/Entity$ScannedEntity;II)V"
            )
        ),
        "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl" => matches!(
            (method_name, descriptor),
            ("getNodeName", "()Ljava/lang/String;")
                | ("getNamespaceURI", "()Ljava/lang/String;")
                | ("getPrefix", "()Ljava/lang/String;")
                | ("getLocalName", "()Ljava/lang/String;")
                | ("getNodeType", "()S")
                | ("getReadOnly", "()Z")
        ),
        "com/sun/org/apache/xerces/internal/impl/xs/opti/ElementImpl" => {
            matches!((method_name, descriptor), ("getTagName", "()Ljava/lang/String;"))
        }
        "com/sun/org/apache/xerces/internal/impl/xs/opti/AttrImpl" => matches!(
            (method_name, descriptor),
            ("getName", "()Ljava/lang/String;")
                | ("getValue", "()Ljava/lang/String;")
                | ("getNodeValue", "()Ljava/lang/String;")
                | ("getSpecified", "()Z")
                | ("isId", "()Z")
        ),
        "com/sun/org/apache/xerces/internal/impl/xpath/regex/RangeToken" => {
            matches!((method_name, descriptor), ("sortRanges", "()V"))
        }
        _ => false,
    }
}

pub(crate) fn is_awt_imageio_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // java.awt.image.BufferedImage side-table raster. CratonVM stores pixels in
    // native-awt's ARGB registry and stamps the Java object with an imageId;
    // the real JDK methods expect populated Raster/ColorModel internals.
    if class_name == "java/awt/image/BufferedImage"
        && matches!(
            (method_name, descriptor),
            ("<init>", "(III)V")
                | ("getWidth", "()I")
                | ("getHeight", "()I")
                | ("getRGB", "(II)I")
                | ("setRGB", "(III)V")
                | ("getType", "()I")
                | ("createGraphics", "()Ljava/awt/Graphics2D;")
                | ("flush", "()V")
                | ("getRGB", "(IIII[III)[I")
        )
    {
        return true;
    }

    // javax.imageio.ImageIO codec bridge. The real-JDK ImageIO SPI expects
    // raster/color-model internals that CratonVM's memory-backed BufferedImage
    // does not populate. Force the native bridge so Spring and desktop code can
    // read/write the ARGB side-table image data through PNG/JPEG codecs.
    if class_name == "javax/imageio/ImageIO"
        && matches!(
            (method_name, descriptor),
            (
                "read",
                "(Ljava/io/InputStream;)Ljava/awt/image/BufferedImage;"
            ) | ("read", "(Ljava/io/File;)Ljava/awt/image/BufferedImage;")
                | (
                    "write",
                    "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/OutputStream;)Z"
                )
                | (
                    "write",
                    "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljavax/imageio/stream/ImageOutputStream;)Z"
                )
                | (
                    "write",
                    "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/File;)Z"
                )
        )
    {
        return true;
    }

    if class_name == "com/sun/imageio/plugins/jpeg/JPEGImageReader"
        && matches!(
            (method_name, descriptor),
            (
                "read",
                "(ILjavax/imageio/ImageReadParam;)Ljava/awt/image/BufferedImage;"
            ) | ("dispose", "()V")
        )
    {
        return true;
    }

    class_name == "com/sun/imageio/plugins/png/PNGImageWriter"
        && method_name == "write"
        && descriptor
            == "(Ljavax/imageio/metadata/IIOMetadata;Ljavax/imageio/IIOImage;Ljavax/imageio/ImageWriteParam;)V"
}

pub(crate) fn is_liquibase_checksum_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        (class_name, method_name, descriptor),
        (
            "liquibase/change/AbstractChange$1",
            "include",
            "(Ljava/lang/Object;Ljava/lang/String;Ljava/lang/Object;)Z",
        ) | (
            "liquibase/change/ColumnConfig",
            "getSerializableFieldValue",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        )
    )
}

pub(crate) fn is_reflection_factory_serialization_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "sun/reflect/ReflectionFactory" | "jdk/internal/reflect/ReflectionFactory"
    ) && matches!(
        (method_name, descriptor),
        ("getReflectionFactory", "()Lsun/reflect/ReflectionFactory;")
            | (
                "getReflectionFactory",
                "()Ljdk/internal/reflect/ReflectionFactory;"
            )
            | (
                "newConstructorForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;"
            )
            | (
                "newConstructorForSerialization",
                "(Ljava/lang/Class;Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;"
            )
            | (
                "newConstructorForExternalization",
                "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;"
            )
            | (
                "readObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "readObjectNoDataForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "writeObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "readResolveForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "writeReplaceForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "hasStaticInitializerForSerialization",
                "(Ljava/lang/Class;)Z"
            )
    )
}

/// Temporary call-count instrumentation for the silent-hang-no-signature-
/// cluster throughput residual (2026-07-13). Tallies invocations of several
/// suspected interpreter dispatch hot-path functions, reported periodically
/// via `CRATONVM_DBG_HOTPATH_COUNTS=1` — independent of wall-clock timing,
/// so it stays valid signal even on a heavily contended/noisy host.
pub(crate) mod hotpath_counts {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static FORCE_NATIVE_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static RESOLVE_METHOD_REF_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static LOOKUP_LOADER_INITIATED_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static RETARGET_FIELD_CALLS: AtomicU64 = AtomicU64::new(0);
    /// Bytecodes dispatched through the **decoded** handler
    /// (`opcodes::execute_instruction`) — NOT the total executed.
    ///
    /// It was called `TOTAL_INSTRUCTIONS`, and it never counted one. Its only
    /// bump site is the top of `execute_instruction`, which the raw-bytecode
    /// fast path in `execute_frame_from_index` reaches only when it has no arm
    /// for the opcode. Read as a total it under-reports by the fast path's
    /// share — which is the large majority of executed bytecodes — so any
    /// ratio taken against it (per-opcode cost, native-call frequency) came out
    /// inflated by exactly the factor nobody had measured. The name now states
    /// the population it actually samples.
    pub static DECODED_INSTRUCTIONS: AtomicU64 = AtomicU64::new(0);

    pub fn bump(counter: &AtomicU64) {
        if !crate::runtime::env_cache::dbg_hotpath_counts() {
            return;
        }
        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() || n % 1_000_000 == 0 {
            eprintln!(
                "[hotpath-counts] force_native={} resolve_method_ref={} \
                 lookup_loader_initiated={} retarget_field={} decoded_instr={}",
                FORCE_NATIVE_CALLS.load(Ordering::Relaxed),
                RESOLVE_METHOD_REF_CALLS.load(Ordering::Relaxed),
                LOOKUP_LOADER_INITIATED_CALLS.load(Ordering::Relaxed),
                RETARGET_FIELD_CALLS.load(Ordering::Relaxed),
                DECODED_INSTRUCTIONS.load(Ordering::Relaxed),
            );
        }
    }
}

pub(crate) fn is_undertow_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    matches!(
        (class_name, method_name, method_descriptor),
        (
            "io/undertow/Undertow",
            "builder",
            "()Lio/undertow/Undertow$Builder;"
        ) | ("io/undertow/Undertow", "start", "()V")
            | ("io/undertow/Undertow", "stop", "()V")
            | (
                "io/undertow/Undertow$Builder",
                "addHttpListener",
                "(ILjava/lang/String;)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "addHttpsListener",
                "(ILjava/lang/String;Ljavax/net/ssl/SSLContext;)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "setHandler",
                "(Lio/undertow/server/HttpHandler;)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "setSocketOption",
                "(Lorg/xnio/Option;Ljava/lang/Object;)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "setWorkerThreads",
                "(I)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "setIoThreads",
                "(I)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "build",
                "()Lio/undertow/Undertow;"
            )
    )
}

/// JDK-ONLY-WAVE2: the warm-path force-native gate — roughly 55 hard-coded
/// class/method/descriptor branches, every one of them a statement that
/// CratonVM's native beats the real JDK's concrete bytecode. Under contract
/// §1.4 that is exactly the thing `--jdk-only` exists to abolish: a `Bridge` or
/// `SyntheticStub` may not shadow real bytes, and a genuine `Intrinsic` needs
/// no name list because `resolve_dispatch` step 2 already takes it.
///
/// What must replace this function: **nothing**. Under `--jdk-only` every
/// branch is dead, because `resolve_dispatch` step 3 returns `Bytecode` for a
/// method that has `Code`. Under `Compatible` the branches are load-bearing for
/// real boots today, so they come out family by family with their own
/// verification, not in one sweep.
///
/// Two branches below carry their own markers because they cannot be removed on
/// their own: the `java/lang/String` exclusion (paired with the positive form in
/// `vm/src/vm/vm_exec.rs`'s `check_override`) and the `ThreadPoolExecutor`
/// family.
///
/// # THIS LIST IS NOT WHAT DECIDES NATIVE-VS-BYTECODE. Read this before adding
/// a class to it.
///
/// The paragraph above is true of `resolve_dispatch` and **false** as a
/// statement about `--jdk-only` as a whole, and the difference has cost several
/// lanes a wrong inference — `G29-1` §6 called it "the thing most likely to be
/// wrong in my fix". `resolve_dispatch` is not the first site to answer.
/// `try_stackless_invoke` step 1 is, for nearly every call in the VM, and it
/// goes through [`resolve_step1_native`], which passes
/// `resolve_native_dispatch_wave1` a hard-coded `compat_native_wins: true` and
/// a `bytecode_available` of `shadows_bytecode && enforce`, where `enforce` is
/// `env_cache::jdk_only_enforce_shadow_for(class_name)` — **off unless
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` is set, and off by default because arming
/// it takes the corpus from 32/17 to 3/46**. With it off, `bytecode_available`
/// is `false`, the `NativeKind::Bridge` arm returns `Some(NativeBridge(..))`,
/// and the registered native runs *in front of real JDK bytecode* — for any
/// class, on or off this list. Existing bytecode buys one `#[cold]`
/// observation (`record_native_shadow_ran_over_bytecode`) and nothing else.
///
/// So the operative rule is: **under `--jdk-only`, registering a `Bridge` for a
/// triple is by itself sufficient for it to preempt real JDK bytecode.** This
/// list is a *second, later* gate, consulted only by the sites that already
/// resolved a bytecode `Method` without asking the registry — the vtable
/// inline-cache (`dispatch_virtual.rs:767` and `:3432`) and the JIT
/// (`jit_bridge.rs:2748`) — where it converts a would-be `Bytecode` cache entry
/// into a `VirtualNative` one. It is a cache-shape override, not the policy.
///
/// MEASURED 2026-08-17 on `C:/craton/target-rel2/release/cratonvm.exe`
/// (`9964ca733`) against HotSpot 25.0.3+9-LTS, and this is why
/// `java/net/http/HttpHeaders` is deliberately **absent** below:
///
/// * `HttpHeaders` is a real, final JDK class; all five of its readers have
///   `Code`; `net_phase_e.rs` registers all five as `Bridge`; none is on this
///   list. All five run anyway, cold and after 300,000 warm iterations at one
///   call site (inline cache + JIT tier-up): `owns_slot=true`,
///   `overwrote=null`, `invocations=300002`, and `--jdk-only-report` tags every
///   one `bridge-ran-over-bytecode`.
/// * Three independent signatures separate "the native ran" from "the bytecode
///   ran", on a receiver built by the REAL `HttpHeaders.of(Map, BiPredicate)`:
///   `map().getClass()` is `java.util.LinkedHashMap` (the native mints one)
///   where HotSpot says `java.util.Collections$UnmodifiableMap`; `map() ==
///   map()` is `false` (the native mints a fresh one per call) where HotSpot
///   says `true`; and `map().put(..)` is ACCEPTED where HotSpot throws
///   `UnsupportedOperationException`.
///
/// **Adding a real, widely-used JDK class here is therefore not a fix for "my
/// native does not run" — it already does.** The blast radius if you add one
/// anyway: this function is called on every vtable miss and every JIT bind, its
/// result is memoized per `CachedBytecodeMethod`, and a new entry converts that
/// call site's inline cache to `VirtualNative` for **every** receiver of the
/// class, including genuinely real ones the native was never written for. That
/// is the failure the `java/lang/String` arm was deleted for on 2026-08-04 (a
/// method's behaviour started depending on how many times its call site had
/// run) and the one the `ThreadPoolExecutor` arm was deleted for on 2026-08-06.
///
/// Full derivation, both directions of the census, and the probes:
/// `docs/known-issues/jdk-only/G34-1-who-wins-native-or-bytecode-20260817.md`.
pub(super) fn force_native_over_real_jdk_bytecode(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    hotpath_counts::bump(&hotpath_counts::FORCE_NATIVE_CALLS);
    // W7-96 §7 NOMINATION 1 — THE RETIREMENT DIAL HAS TO WIN HERE.
    //
    // `CRATONVM_ENFORCE_NATIVE_SHADOW` is the instrument the whole Phase 2
    // migration is supposed to be measured with: arm it for a class and that
    // class's registered natives are supposed to step aside so the real JDK
    // bytecode runs and can be judged. This function never consulted it, so
    // arming the dial measured THIS GATE rather than the VM.
    //
    // W7-96 §3.1 measured the consequence, 3 of 3 runs identical. Armed for
    // `java/util/concurrent/ConcurrentHashMap`, six `put`s of distinct keys
    // into a fresh map gave `size() == 1` and `get()` hits 1 of 6 — five
    // entries silently gone, with `put` reporting a fresh insert for every one.
    // Not a real-bytecode CAS bug: `<init>` is absent from the collection
    // cluster below while `put` is in it, so the dial retired the constructor
    // and kept the mutators, leaving a map with no segments whose
    // `chm_segment_for -> None` arm answers "no previous mapping" having stored
    // nothing. The record spent an hour reading that as a VM defect.
    //
    // Placed at the TOP rather than on the one cluster the record names,
    // because every rule below has the same problem and a per-cluster guard
    // would have to be repeated correctly in three files. `EnforceShadowScope`
    // is `Off` unless the env var is set, and `Off::covers()` is `false`, so an
    // ordinary run does not change by one dispatch.
    if crate::runtime::env_cache::enforce_shadow_scope().covers(class_name) {
        return false;
    }
    // `Map.values()` (native_map_values in native-collections/src/lib.rs)
    // returns a plain `java/util/ArrayList` that stashes its source map in a
    // spare trailing capacity slot so a later `Map.put`/`remove` on the
    // source is reflected on read (`resync_values_view`, called from
    // `native_al_size`/`native_al_is_empty`/`native_al_get`/
    // `native_al_contains`/`native_al_iterator`/`native_al_to_array*` etc.).
    // Real `ArrayList` bytecode declares its own `size()`/`isEmpty()`/`get()`/
    // etc., so without this force-entry the receiver-has-own-bytecode rule
    // picks real bytecode over the registered native, skipping the resync
    // entirely and freezing the returned Collection at whatever the source
    // map held at `values()` call time (H2 `TestAlter.
    // testAlterTableDropIdentityColumn`: `Schema.getAllSequences()` captures
    // `ConcurrentHashMap.values()` once, before any sequence exists).
    // `resync_values_view` itself is a cheap no-op for an ordinary ArrayList
    // (no stashed source map in the trailing slot), so forcing these methods
    // through the native is safe for plain ArrayLists too.
    if class_name == "java/util/ArrayList"
        && matches!(
            method_name,
            "size"
                | "isEmpty"
                | "get"
                | "contains"
                | "iterator"
                | "toArray"
                | "indexOf"
                | "lastIndexOf"
                | "toString"
                | "hashCode"
                | "equals"
        )
    {
        return true;
    }
    // Same argument, for the classes a map view is now minted under
    // (native-collections' `MAP_VIEW_CARRIERS`). These are REAL JDK classes,
    // and their real bodies read `this$0` — which a CratonVM-minted view
    // leaves null, because the view's state lives in ArrayList's own
    // `elementData`/`size` slots instead. Every method the carriers declare
    // themselves has to reach the registered native or it runs a JDK body over
    // a layout that is not the JDK's.
    //
    // `equals`/`hashCode` are deliberately absent: the JDK's views inherit
    // `AbstractCollection`'s identity semantics, which is what CratonVM should
    // answer too, and there is no native registered for them on these classes
    // (see `register_map_view_carrier_natives`).
    if matches!(
        class_name,
        "java/util/HashMap$Values"
            | "java/util/LinkedHashMap$LinkedValues"
            | "java/util/TreeMap$Values"
            | "java/util/TreeMap$EntrySet"
            | "java/util/Hashtable$ValueCollection"
            | "java/util/concurrent/ConcurrentHashMap$ValuesView"
    ) && matches!(
        method_name,
        "size"
            | "isEmpty"
            | "contains"
            | "iterator"
            | "toArray"
            | "toString"
            | "remove"
            | "clear"
            | "forEach"
            | "stream"
            | "removeIf"
            | "spliterator"
    ) {
        return true;
    }
    // The same again for the SET-shaped views (native-collections'
    // `SET_VIEW_CARRIERS`). Here `equals`/`hashCode` ARE included: these
    // carriers extend `AbstractSet`, whose `equals`/`hashCode` are the SET
    // contract, and `register_set_view_carrier_natives` registers
    // `native_hs_equals`/`native_hs_hash_code` for exactly that reason — the
    // JDK bodies would compute it off a null `this$0`.
    if matches!(
        class_name,
        "java/util/HashMap$KeySet"
            | "java/util/HashMap$EntrySet"
            | "java/util/LinkedHashMap$LinkedKeySet"
            | "java/util/LinkedHashMap$LinkedEntrySet"
            | "java/util/Hashtable$KeySet"
            | "java/util/Hashtable$EntrySet"
            | "java/util/TreeMap$KeySet"
            | "java/util/concurrent/ConcurrentHashMap$EntrySetView"
    ) && matches!(
        method_name,
        "size"
            | "isEmpty"
            | "add"
            | "contains"
            | "iterator"
            | "toArray"
            | "toString"
            | "remove"
            | "clear"
            | "forEach"
            | "stream"
            | "removeIf"
            | "spliterator"
            | "addAll"
            | "removeAll"
            | "retainAll"
            | "containsAll"
            | "equals"
            | "hashCode"
            | "first"
            | "last"
            | "comparator"
            | "headSet"
            | "tailSet"
            | "subSet"
            | "descendingIterator"
            | "descendingSet"
            | "pollFirst"
            | "pollLast"
            | "ceiling"
            | "floor"
            | "higher"
            | "lower"
    ) {
        return true;
    }
    // And for the sublist carrier (native-collections' `ASL_REAL_CLASS`). A
    // CratonVM-minted view carries `(parent, offset, size, expected,
    // viewParent)` PAST the class's own `root`/`parent`/`offset`/`size`
    // (`asl_base`), so the real bodies would read fields this VM never fills
    // and report the view empty. Unlike the carriers above, this class is ALSO
    // instantiated by java.base's own `ArrayList.subList`, and forcing the
    // native for those would be the mirror-image silent-empty bug — which is
    // why every `native_asl_*` opens with `asl_delegate_foreign` and hands a
    // receiver it did not mint straight back to this bytecode.
    if class_name == "java/util/ArrayList$SubList"
        && matches!(
            method_name,
            "size"
                | "isEmpty"
                | "get"
                | "set"
                | "iterator"
                | "listIterator"
                | "toArray"
                | "toString"
                | "contains"
                | "containsAll"
                | "indexOf"
                | "lastIndexOf"
                | "stream"
                | "forEach"
                | "spliterator"
                | "hashCode"
                | "equals"
                | "subList"
                | "add"
                | "remove"
                | "clear"
                | "addAll"
                | "removeIf"
                | "sort"
                | "removeAll"
                | "retainAll"
                | "replaceAll"
                | "parallelStream"
                | "getFirst"
                | "getLast"
                | "addFirst"
                | "addLast"
                | "removeFirst"
                | "removeLast"
                | "reversed"
        )
    {
        return true;
    }
    // And for the iterator carriers (native-collections'
    // `MAP_KEY_ITR_CARRIERS`). A `HashMap$KeyIterator`'s own bytecode walks the
    // `next`/`current`/`index` fields `HashIterator` declares, and a
    // CratonVM-minted one carries a SNAPSHOT past those slots instead
    // (`key_itr_base`), so the real bodies would report every collection
    // exhausted.
    // `setValue` on a LIVE entrySet element. It carries its source map in a
    // trailing undeclared slot and the JDK body writes only the field, so
    // `entrySet()...setValue(v)` would stop updating the map. Scoped to the one
    // method: `getKey`/`getValue`/`equals`/`hashCode`/`toString` read the
    // declared slots, which a live entry fills correctly, and their real
    // bytecode is the better answer.
    if matches!(
        class_name,
        "java/util/HashMap$Node"
            | "java/util/LinkedHashMap$Entry"
            | "java/util/TreeMap$Entry"
            | "java/util/concurrent/ConcurrentHashMap$MapEntry"
            | "java/util/Hashtable$Entry"
    ) && method_name == "setValue"
    {
        return true;
    }
    if matches!(
        class_name,
        "java/util/HashMap$KeyIterator"
            | "java/util/HashMap$EntryIterator"
            | "java/util/LinkedHashMap$LinkedKeyIterator"
            | "java/util/LinkedHashMap$LinkedEntryIterator"
            // `TreeMap$KeyIterator` joined 2026-08-22 -- it is what HotSpot
            // answers for BOTH `TreeSet.iterator()` and
            // `TreeMap.keySet().iterator()`, where this VM was handing back the
            // `Arrays$ArrayItr` refusal landing. Its snapshot lives at
            // `key_itr_base`, past `PrivateEntryIterator`'s own fields, so the
            // real bodies would walk a `next` chain nothing populated and
            // report every set exhausted. Single-producer class, so this row
            // cannot capture anyone else's objects -- unlike the
            // `Hashtable$Enumerator` attempt that reddened `RJdkEnumerations`.
            // The values-side twins, 2026-08-22. `values()` is a `Collection`,
            // not a `Set`, so it is ArrayList-shaped and its iterator came back
            // as a plain `ArrayList$Itr` where HotSpot names a per-family
            // class. Their three snapshot fields live past the class's own
            // declared ones (`al_itr_alt_base`), so without these rows the real
            // bodies walk fields nothing populated. Each is single-producer.
            | "java/util/HashMap$ValueIterator"
            | "java/util/LinkedHashMap$LinkedValueIterator"
            | "java/util/TreeMap$ValueIterator"
            | "java/util/TreeMap$EntryIterator"
            // The ConcurrentHashMap views, 2026-08-28. `keySet().iterator()`
            // used to mint the FABRICATED `java/util/HashMap$KeyItr` in
            // compatible mode and land on `Arrays$ArrayItr` when `--jdk-only`
            // refused it, so one receiver answered two different wrong class
            // names depending on the mode. Both are now the real per-family
            // class, and both are single-producer: `chm_real_dual_iterator` is
            // gone, so nothing else mints either of them (which is the
            // condition the `Hashtable$Enumerator` and the CHM$ValueIterator
            // attempts each failed).
            | "java/util/concurrent/ConcurrentHashMap$KeyIterator"
            | "java/util/concurrent/ConcurrentHashMap$EntryIterator"
            // The two remaining collection families, 2026-08-29 (L3 residual
            // 6.1). `TreeMap$KeyIterator` above already covers `TreeSet` and
            // `TreeMap.keySet()`; these are the other two receivers that were
            // handing out a class HotSpot does not name -- `ArrayDeque` and
            // `PriorityQueue` answered `Arrays$ArrayItr` and `ArrayList$Itr`.
            //
            // WITHOUT THESE ROWS THE REGISTRATION IS SILENT, and it fails in
            // the most confusing available way: the real bodies run, walk
            // declared fields nothing populated, and report the collection
            // EXHAUSTED. Measured exactly that on the first build --
            // `PqOptionalShadowSweep` died at row 54 with
            // `NoSuchElementException: No more elements` from a freshly minted
            // iterator over a three-element queue. The inverse of
            // `a-force-native-gate-entry-with-no-registration-is-silent`, and
            // the same lesson: a registration and its gate row are one edit.
            //
            // Each is single-producer -- `native_ad_iterator` and
            // `native_pq_iterator` are the only mint sites, and both families'
            // `iterator()` is itself overridden, so no bytecode path can
            // present an object of either class to these natives.
            | "java/util/PriorityQueue$Itr"
    ) && matches!(method_name, "hasNext" | "next" | "remove")
    {
        return true;
    }
    // BUG (found investigating the Tomcat Jasper/ecj JSP-compile NPE,
    // TestDefaultServlet.testBug57601 / TestMapperWebapps.testWelcomeFileStrict):
    // this function is the ONLY force-native gate consulted by the
    // reflective/megamorphic/`invokespecial`/interface-default dispatch path
    // (`intercept_force_registered_native[_cached]` ->
    // `should_force_registered_native_over_bytecode` ->
    // `force_native_over_real_jdk_bytecode_memoized` -> here). The "regular"
    // cached-invokevirtual dispatch path OR's in an extra
    // `matches!((class_name,...), "java/util/HashMap"|"java/util/LinkedHashMap"
    // |"java/util/Hashtable"|"java/util/concurrent/ConcurrentHashMap")`
    // cluster locally (see further below in this same file, and the
    // companion `check_override` chain in `vm/src/vm/vm_exec.rs`), but this
    // base function never did — so a call reaching it directly ran the REAL
    // JDK bytecode for `put`/`get`/`size`/etc. instead of (or, when a
    // different call to the identical call site had already gone through the
    // OTHER, covered path, *in addition to*) the registered native,
    // corrupting any state the two implementations don't share (e.g.
    // `Hashtable`'s own real `count` field vs. our side-store bucket count —
    // `Hashtable.put()` ending up incrementing the tracked size TWICE,
    // doubling `size()` and leaving `values().toArray()`'s caller-supplied
    // array null-padded past the real entry count. That is exactly what made
    // ecj's `CompilationResult.getClassFiles()` — `new
    // ClassFile[compiledTypes.size()]` then `compiledTypes.values()
    // .toArray(classFiles)` on a `Hashtable(11)` — hand back a null-padded
    // array and NPE in `CompilationUnitDeclaration.cleanUp()`). Add the same
    // cluster here so every dispatch path agrees.
    if matches!(
        class_name,
        "java/util/HashMap"
            | "java/util/LinkedHashMap"
            | "java/util/Hashtable"
            | "java/util/concurrent/ConcurrentHashMap"
    ) && matches!(
        method_name,
        "computeIfAbsent"
            | "compute"
            | "computeIfPresent"
            | "merge"
            | "putIfAbsent"
            | "replace"
            | "forEach"
            | "replaceAll"
            | "getOrDefault"
            | "putMapEntries"
            | "put"
            | "get"
            | "remove"
            | "containsKey"
            | "containsValue"
            | "size"
            | "isEmpty"
            | "clear"
            | "putAll"
            | "keySet"
            | "values"
            | "entrySet"
            | "keys"
            | "elements"
    ) {
        return true;
    }
    // `ConcurrentHashMap$KeySetView` — the object `newKeySet()` / `keySet(V)`
    // hands back is a real KeySetView over a native-backed ConcurrentHashMap,
    // whose entries live in CratonVM's segmented layout rather than the `table`
    // field. Every method listed here has a real body that reads `table`
    // directly (`add` via `putVal`, `iterator`/`forEach`/`spliterator` via a
    // `Traverser`, `hashCode`/`equals` via the iterator), so it must run the
    // native instead. The methods NOT listed are the ones `CollectionView`
    // declares in terms of `map` or `iterator()` — `size`/`isEmpty`/`clear`/
    // `toArray`/`toString`/`containsAll`/`removeAll`/`retainAll`; their real
    // bodies are correct once these are native, and forcing them here would
    // also capture `ValuesView`/`EntrySetView`, which share that declaring
    // class but not these semantics. See the retired
    // `concurrenthashmap-newkeyset-returns-a-plain-hashset` write-up.
    if class_name == "java/util/concurrent/ConcurrentHashMap$KeySetView"
        && matches!(
            method_name,
            "add"
                | "addAll"
                | "remove"
                | "contains"
                | "iterator"
                | "forEach"
                | "spliterator"
                | "stream"
                | "hashCode"
                | "equals"
                | "removeIf"
                | "getMappedValue"
                | "getMap"
        )
    {
        return true;
    }
    // ConcurrentHashMap's private serialization hooks. CratonVM stores CHM
    // entries in a segmented native layout, so the real JDK `writeObject`
    // (which walks the always-null `table`) serialised every CHM as empty and
    // the real `readObject` rebuilt a `table` our natives never read. Both are
    // reached through `ObjectStreamClass.invokeWriteObject`/`invokeReadObject`,
    // i.e. reflective `Method.invoke` -- a path with no bytecode PC to key an
    // invoke-cache entry on, so it consults this gate directly. Kept separate
    // from the Map cluster above because HashMap/LinkedHashMap/Hashtable have
    // no such natives and must keep running their real bodies.
    if class_name == "java/util/concurrent/ConcurrentHashMap"
        && matches!(method_name, "writeObject" | "readObject")
    {
        return true;
    }
    // The serialization hooks of the immutable-collection carriers, for the
    // same reason one class over. `List.of`/`Set.of`/`Map.of`/`copyOf` return a
    // CratonVM-minted object whose `getClass()` aliases to one of these six
    // real JDK classes, and each of them declares its own `writeReplace()` -
    // a body that reads `e0`/`e1`/`elements`/`table`, the fields the carrier
    // does not populate. Left to the real bytecode it wrote a `java.util.
    // CollSer` holding this VM's `(backing, immutable-marker)` slot pair, and
    // reading that back threw `InvalidObjectException: invalid object` (20 of
    // the 52 rows of `probes/CollectionSerProbe.java`; Spring's
    // `AnnotationTransactionAttributeSourceTests.serializable()` is the suite
    // case). `CollSer.readResolve` is here too because its `IMM_MAP` arm builds
    // through `new MapN<>(array)` - a real constructor whose `table` none of
    // the map natives read. `Collections$UnmodifiableRandomAccessList` is the
    // one non-immutable member: it is the only `Collections$Unmodifiable*`
    // wrapper that declares a `writeReplace`, and its native declines the
    // replacement so it joins the siblings that already round-trip.
    // Registrations: `register_immutable_serialization_natives` in
    // native-collections. Companion entry in the `vm_exec` twin.
    if matches!(
        class_name,
        "java/util/ImmutableCollections$List12"
            | "java/util/ImmutableCollections$ListN"
            | "java/util/ImmutableCollections$Set12"
            | "java/util/ImmutableCollections$SetN"
            | "java/util/ImmutableCollections$Map1"
            | "java/util/ImmutableCollections$MapN"
            | "java/util/Collections$UnmodifiableRandomAccessList"
    ) && method_name == "writeReplace"
    {
        return true;
    }
    if class_name == "java/util/CollSer" && method_name == "readResolve" {
        return true;
    }
    // Keep this warmed-invoke-cache policy in sync with vm_exec's cold-path
    // allow-list. JarFile inherits these operations from ZipFile, so a
    // subclass `super.close()` resolves to the real ZipFile bytecode after
    // cache population unless its registered bridge is forced here too. The
    // real body dereferences constructor state which native-backed JarFiles do
    // not have.
    if class_name == "java/util/zip/ZipFile"
        && matches!(
            method_name,
            "<init>"
                | "getInputStream"
                | "entries"
                | "stream"
                | "getComment"
                | "close"
                | "getName"
                | "isMultiRelease"
                | "size"
        )
    {
        return true;
    }
    // Keep in sync with vm_exec.rs's cold-path gate. The real JDK builder
    // methods read compact-string fields that do not exist on our synthetic
    // char[]-backed builders, so all registered layout operations must resolve
    // through their native implementations.
    if is_string_builder_layout_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_undertow_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // BUG-W follow-up (2026-07-20): `java.lang.ClassValue.get()` has real JDK
    // bytecode (relies on `Class.classValueMap`, which CratonVM's Class
    // mirrors don't back) AND a registered native override (memoized
    // `computeValue` dispatch — see the `get()`/`remove()` registrations in
    // `native-builtins/src/phases_late.rs`). Any cached/precomputed dispatch
    // decision that consults this allow-list instead of re-walking the
    // ancestor chain at call time (the JIT's compiled-callsite native check,
    // mirroring the interpreter's `try_stackless_invoke`/`invoke_or_native`
    // walk) needs an explicit entry here or it silently keeps running the
    // real bytecode forever, which is how Groovy's `ClassInfo.getClassInfo`
    // NPE'd under `-Jit on` even after the native fix landed.
    if class_name == "java/lang/ClassValue"
        && method_name == "get"
        && method_descriptor == "(Ljava/lang/Class;)Ljava/lang/Object;"
    {
        return true;
    }
    if class_name == "org/springframework/core/annotation/MergedAnnotation$Adapt"
        && method_name == "isIn"
        && method_descriptor == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
    {
        return true;
    }
    // Real-JDK constant-surface bridges. Keep in sync with vm_exec.rs.
    if matches!(
        (class_name, method_name, method_descriptor),
        ("java/nio/charset/Charset", "contains", "(Ljava/nio/charset/Charset;)Z")
            | ("java/nio/file/Files", "getOwner", "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/UserPrincipal;")
            | ("java/lang/StackFrameInfo", "getMethodType", "()Ljava/lang/invoke/MethodType;")
            | ("java/net/DatagramSocket", "<init>", "()V")
            | ("java/net/DatagramSocket", "<init>", "(I)V")
            | ("java/net/DatagramSocket", "<init>", "(ILjava/net/InetAddress;)V")
            | ("java/net/DatagramSocket", "connect", "(Ljava/net/InetAddress;I)V")
            | ("java/net/DatagramSocket", "disconnect", "()V")
            | ("java/lang/StackWalker$StackFrame", "getMethodType", "()Ljava/lang/invoke/MethodType;")
            | ("java/lang/StackWalker$StackFrame", "getDescriptor", "()Ljava/lang/String;")
    ) {
        return true;
    }
    // JDK 25's public Class.getProtectionDomain() reads a VM-populated private
    // mirror field directly. CratonVM's mirrors retain class provenance in the
    // class store instead, so force the registered class-id-backed native.
    if class_name == "java/lang/Class"
        && matches!(
            (method_name, method_descriptor),
            ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
                // JDK 25 implements isArray() as a direct read of the
                // private componentType field. Array mirrors keep their
                // identity in the class store, so that bytecode falsely
                // reports `Class[]` as a non-array and Spring skips its
                // Class[] -> String[] annotation adaptation.
                | ("isArray", "()Z")
                | ("getComponentType", "()Ljava/lang/Class;")
                // Spring's annotation map adapter uses the package-private
                // alias rather than the public accessor.
                | ("componentType", "()Ljava/lang/Class;")
        )
    {
        return true;
    }
    if is_springboot_mongo_reactive_customizer_destroy_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    if is_springboot_mongo_reactive_customizer_customize_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    if is_datagram_channel_open_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Tomcat application methods are never registered native overrides apart
    // from the audited bridges below. Reject the large compatibility table
    // early on its hot scanner paths.
    if (class_name.starts_with("org/apache/")
        && !matches!(
            class_name,
            "org/apache/maven/surefire/booter/ForkedBooter"
                | "org/apache/tomcat/util/buf/CharChunk"
                | "org/apache/catalina/connector/Response"
                | "org/apache/tomcat/util/bcel/classfile/Constant"
        ))
        || class_name == "java/net/URI"
    {
        return false;
    }
    if class_name == "org/apache/catalina/connector/Response"
        && method_name == "toAbsolute"
        && method_descriptor == "(Ljava/lang/String;)Ljava/lang/String;"
    {
        return true;
    }
    // Only `toString` is listed here, and only because it has a matching
    // registration (`native_char_chunk_to_string`). This gate used to also
    // claim `endsWith(String)`, `indexOf(char)` and
    // `AbstractChunk.indexOf(String,III)`, none of which were ever
    // registered — every consumer resolves the callback through
    // `NativeMethodRegistry::find` and silently falls back to bytecode when
    // it misses, so those three were pure dead config that read as "served
    // by a native" to anyone auditing this list. If natives are added for
    // them later, BOTH this gate and the `CharChunk` registrations in
    // `native-builtins`' `register_essential_natives_with_shims` must be
    // updated together.
    if class_name == "org/apache/tomcat/util/buf/CharChunk"
        && (method_name, method_descriptor) == ("toString", "()Ljava/lang/String;")
    {
        return true;
    }
    if class_name == "org/apache/tomcat/util/bcel/classfile/Constant"
        && method_name == "readConstant"
        && method_descriptor
            == "(Ljava/io/DataInput;)Lorg/apache/tomcat/util/bcel/classfile/Constant;"
    {
        return true;
    }
    // 995ff48c (Tomcat silent-hang scanner fix): interpreted per-byte read
    // dispatch dominated the scanner's hot path. `<init>`/mark/reset/skip/...
    // still run their real-JDK bytecode so buffer/mark state stays
    // bytecode-owned; only the two read overloads are forced native.
    if class_name == "java/io/BufferedInputStream"
        && method_name == "read"
        && matches!(method_descriptor, "([BII)I" | "()I")
    {
        return true;
    }
    if class_name == "java/io/DataInputStream"
        && matches!(
            (method_name, method_descriptor),
            ("readUTF", "()Ljava/lang/String;")
                | ("readByte", "()B")
                | ("readUnsignedByte", "()I")
                | ("readUnsignedShort", "()I")
                | ("readInt", "()I")
                | ("readLong", "()J")
                | ("readFloat", "()F")
                | ("readDouble", "()D")
                | ("skipBytes", "(I)I")
        )
    {
        return true;
    }
    if class_name == "java/io/FileInputStream"
        && method_name == "read"
        && method_descriptor == "([BII)I"
    {
        return true;
    }
    // Real JDK CRC32.updateBytes is a small validation wrapper around the
    // registered updateBytes0 native. Keep that boundary native in every
    // dispatch mode: compiled archive writers otherwise risk applying the
    // public CRC representation as the complemented running state.
    if class_name == "java/util/zip/CRC32"
        && method_name == "updateBytes"
        && method_descriptor == "(I[BII)I"
    {
        return true;
    }
    if class_name == "java/io/File"
        && matches!(
            (method_name, method_descriptor),
            ("isDirectory", "()Z")
                | ("list", "()[Ljava/lang/String;")
                | ("getName", "()Ljava/lang/String;")
                | ("canRead", "()Z")
        )
    {
        return true;
    }
    if class_name == "java/lang/StringUTF16"
        && method_name == "getChars"
        && method_descriptor == "([BII[CI)V"
    {
        return true;
    }
    // The forced-native `java/lang/String` policy — the twelve-shape whitelist
    // that stood here, its inverted `return false` exclusion, and the two
    // blocks below it that named `substring`/`charAt`/`length`/`isEmpty`/
    // `startsWith` — IS GONE, together with `check_override`'s 21-name
    // positive twin and the JIT's `String.toLowerCase(Locale)` ladder.
    //
    // Removed because it was MEASURED inert, not because it looked redundant
    // (that reasoning is what item 8 cost a session). See
    // `forced_native_string_policy_is_not_reintroduced` below for the guard,
    // and
    // `forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`
    // for the measurement: with both lists deleted the 392-case `String`
    // matrix was byte-identical in both modes and every one of the 38
    // exercised `java/lang/String` registry slots reported an unchanged
    // invocation count.
    //
    // Neither list ever decided anything. `resolve_step1_native`
    // (`try_stackless_invoke` step 1) resolves the triple in the registry and
    // dispatches whatever it finds, with no list, before either of these runs.
    // The real decision was always "is a native registered for this triple in
    // real-JDK mode", so that is where the policy now lives — see
    // `NativeMethodRegistry::register`'s `java/lang/String` real-JDK drop.
    if is_class_mirror_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_classvalue_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // JFR's Type bootstrap table compares Class mirrors by reference.  A
    // bootstrap type can reach this point through a separately materialised
    // mirror, so run the registered bridge which canonicalises through the VM
    // ClassId before delegating to JFR's String-keyed lookup.
    if is_jfr_metadata_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Reflective Method mirrors may expose stale physical returnType slots in
    // the real JDK. The registered accessor derives the answer from the
    // member descriptor, which is authoritative for JFR annotation metadata.
    if class_name == "java/lang/reflect/Method"
        && method_name == "getReturnType"
        && method_descriptor == "()Ljava/lang/Class;"
    {
        return true;
    }
    // The platform-server bridge returns a synthetic MBeanServer receiver.
    // Interface call sites must select its registered bridge methods rather
    // than executing the abstract interface declarations.
    if class_name == "javax/management/MBeanServer" {
        return true;
    }
    // `java/util/Collections.emptyList()`.
    //
    // The comment that stood here said: "The real Collections.emptyList()
    // returns the class's pre-built static singleton. During the Brave
    // bootstrap that slot can retain a polluted ArrayList, so use the
    // registered constructor-backed empty-list native instead of exposing that
    // stale shared state."
    //
    // Only its first sentence is still true, and the rest is self-refuting
    // against the native it routes to. `native_collections_empty_list`
    // (`native-collections/src/lib.rs`) begins with
    // `collections_empty_singleton(ctx, "EMPTY_LIST")`, which is a
    // `get_static_field(java/util/Collections, EMPTY_LIST)` — it READS the very
    // slot the comment claims this arm exists to avoid. If that slot held a
    // polluted `ArrayList`, this arm would hand the pollution straight back. It
    // cannot deliver the protection it advertised, and could not on the day it
    // was written unless the native looked different then.
    //
    // The hazard itself was real and was fixed AT ITS SOURCE, elsewhere:
    // `ensure_collections_empty_singletons` used to seed `EMPTY_LIST` with an
    // ordinary MUTABLE synthetic `java/util/ArrayList`, so
    // `emptyList() instanceof ArrayList` was true, kotlin-reflect's shaded
    // protobuf `SmallSortedMap.ensureEntryArrayMutable` skipped its replacement
    // step on the strength of that, and mutated the process-wide singleton. It
    // now seeds the real immutable `Collections$Empty*` instances. That is where
    // "a polluted ArrayList in the slot" was closed; this arm never closed it.
    //
    // WHAT THE ARM ACTUALLY DOES TODAY is the native's SECOND half: when
    // `EMPTY_LIST` is not yet initialised, fabricate a fresh empty list rather
    // than returning null. Note that fallback diverges from the oracle on all
    // three properties measured on HotSpot 25.0.3:
    //     class            = java.util.Collections$EmptyList   (fallback: ArrayList)
    //     add("x")         = UnsupportedOperationException     (fallback: ACCEPTED)
    //     two calls same   = true                              (fallback: fresh each call)
    // so the arm buys bootstrap-order robustness and pays for it in fidelity.
    //
    // DO NOT DELETE THIS AS A ONE-LINER. Two things have to be established
    // first, and neither is done:
    //
    // 1. Whether this arm decides anything at all. `resolve_step1_native`
    //    resolves the triple in the registry and dispatches what it finds
    //    BEFORE this function runs — that is exactly why the twelve-shape
    //    forced-native `java/lang/String` policy above turned out to be
    //    measured inert and was deleted. `Collections.emptyList` is a live
    //    `Bridge` registration (`native-collections/src/lib.rs`,
    //    `register_collections_utility_natives`), so the same question applies
    //    and has not been asked.
    // 2. Removing it is a PAIR, not a line. Handing the method back to real JDK
    //    bytecode also needs the triple in `RETIRED_SHADOW_TRIPLES`
    //    (`native-api/src/retired_shadow.rs`) — it is not there today. Dropping
    //    this arm alone leaves the registered native winning by ordinary
    //    dispatch and changes nothing; adding the table entry alone leaves this
    //    arm forcing the native over the bytecode. Neither half is useful on
    //    its own, and that file's own rule applies: a class's state has to
    //    become real before its shadow can be retired.
    if class_name == "java/util/Collections"
        && method_name == "emptyList"
        && method_descriptor == "()Ljava/util/List;"
    {
        return true;
    }

    // TOMBSTONE (H3-1, 2026-08-20). An arm here forced the native over real
    // bytecode for `Predicate.{and,or,negate,not}`. Those four registrations
    // are DELETED -- `javap` on JDK 25 shows none of the four is `ACC_NATIVE`
    // and all four have real bodies, and `RJdkFunctionCombinators` already
    // passes under `--jdk-only`, where they were dropped anyway.
    //
    // The arm was already INERT rather than fatal: of its three consulting
    // sites, two re-check the registry (so an unregistered triple falls
    // through) and the JIT one only seals the method out of compilation. It is
    // removed because a force-native table naming triples that no longer exist
    // is a lie about the tree, and the next reader would have to re-derive that
    // it is harmless.
    //
    // NOTE: the `Collections.emptyList` arm above carries a rule that removing
    // it is a PAIR needing a `RETIRED_SHADOW_TRIPLES` entry. That rule does NOT
    // apply here -- it is for a live `Bridge` being handed back to bytecode.
    // These four were `SyntheticStub` (never `allowed_in(JdkOnly)`) and are now
    // unregistered outright, so there is nothing to re-tag.
    // docs/known-issues/jdk-only/H3-1-the-ratchet-that-did-not-compile-20260820.md
    // The real DecimalFormatSymbols factories enter CLDR's locale bootstrap.
    // During the early Spring/JUnit summary path that bootstrap can observe a
    // stale Collections empty-list slot, producing a type-correct but wrong
    // List element.  The registered locale native constructs the same DFS
    // instance without that provider walk; force it over the concrete JDK
    // bytecode on every interpreter dispatch path.
    if class_name == "java/text/DecimalFormatSymbols"
        && matches!(
            (method_name, method_descriptor),
            ("initialize", "(Ljava/util/Locale;)V")
                | (
                    "getInstance",
                    "(Ljava/util/Locale;)Ljava/text/DecimalFormatSymbols;"
                )
        )
    {
        return true;
    }
    // Base64 encoders are represented by VM-side synthetic state.  The real
    // JDK bytecode instead reads its private object layout, which is not
    // populated for those synthetic instances and silently falls back to the
    // basic, padded encoding.  Keep this in sync with vm_exec's slow-path
    // override gate so warmed invoke caches also use the native implementation.
    if matches!(
        class_name,
        "java/util/Base64" | "java/util/Base64$Encoder" | "java/util/Base64$Decoder"
    ) {
        return true;
    }

    if class_name == "java/lang/Object"
        && method_name == "clone"
        && method_descriptor == "()Ljava/lang/Object;"
    {
        return true;
    }
    // Mockito's ModuleMemberAccessor selects an instrumentation-backed Java-9
    // implementation. The legacy bridge short-circuited that to the reflection
    // fallback on every run — a silent HotSpot divergence that breaks access to
    // strongly-encapsulated members. Off by default; see
    // `flags::mockito_legacy_selectors`.
    if cratonvm_types::flags::mockito_legacy_selectors()
        && class_name == "org/mockito/internal/util/reflection/ModuleMemberAccessor"
        && method_name == "delegate"
        && method_descriptor == "()Lorg/mockito/plugins/MemberAccessor;"
    {
        return true;
    }
    // A real SSLContext returns SunJSSE's concrete factory implementation.
    // The layered Socket overload must still reach the public factory bridge:
    // MockWebServer uses it to wrap its accepted socket as a TLS server.
    if (class_name == "javax/net/ssl/SSLSocketFactory"
        || class_name.starts_with("sun/security/ssl/SSLSocketFactoryImpl"))
        && method_name == "createSocket"
        && matches!(
            method_descriptor,
            "(Ljava/lang/String;I)Ljava/net/Socket;"
                | "(Ljava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;"
        )
    {
        return true;
    }
    if (class_name == "javax/net/ssl/SSLSocket"
        || class_name.starts_with("sun/security/ssl/SSLSocketImpl"))
        && matches!(
            (method_name, method_descriptor),
            ("startHandshake", "()V")
                | ("getInputStream", "()Ljava/io/InputStream;")
                | ("getOutputStream", "()Ljava/io/OutputStream;")
                | ("getSession", "()Ljavax/net/ssl/SSLSession;")
                | ("close", "()V")
                | ("isClosed", "()Z")
                | ("isConnected", "()Z")
                | ("getPort", "()I")
                // The real `javax.net.ssl.SSLSocket` base class's default body
                // for these two just throws `UnsupportedOperationException` —
                // only a concrete provider subclass (SunJSSE's SSLSocketImpl)
                // overrides them. Our synthetic server-side socket (returned
                // by `SSLSocketFactory.createSocket(Socket,...)`, e.g. for
                // MockWebServer's HTTPS listener) IS that class literally, so
                // without forcing native here the real base-class bytecode
                // runs and throws — silently caught+logged at FINE by
                // MockWebServer's connection handler, which then just closes
                // the socket having never read the request or written a
                // response (`skipSslValidation`-style 30s client-side hang).
                | ("getApplicationProtocol", "()Ljava/lang/String;")
                | ("getHandshakeApplicationProtocol", "()Ljava/lang/String;")
                | ("getSSLParameters", "()Ljavax/net/ssl/SSLParameters;")
                | ("setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V")
                | ("setUseClientMode", "(Z)V")
                | ("getUseClientMode", "()Z")
                | ("setNeedClientAuth", "(Z)V")
                | ("getNeedClientAuth", "()Z")
                | ("setWantClientAuth", "(Z)V")
                | ("getWantClientAuth", "()Z")
        )
    {
        return true;
    }
    // SSLContext's real-JDK bodies delegate through a provider-owned
    // SSLContextSpi. CratonVM stores configured key/trust material on the
    // public context object instead, so the native path must own the complete
    // init-to-engine handoff for a server identity to reach Tomcat's engine.
    if (class_name == "javax/net/ssl/SSLContext"
        || class_name.starts_with("sun/security/ssl/SSLContextImpl"))
        && matches!(
            (method_name, method_descriptor),
            ("getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;")
                | ("init", "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V")
                | ("getSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;")
                | ("createSSLEngine", "()Ljavax/net/ssl/SSLEngine;")
                | ("createSSLEngine", "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;")
        )
    {
        return true;
    }
    if class_name == "sun/security/ssl/SSLEngineImpl"
        && matches!(
            (method_name, method_descriptor),
            (
                "setHandshakeApplicationProtocolSelector",
                "(Ljava/util/function/BiFunction;)V"
            ) | (
                "getHandshakeApplicationProtocolSelector",
                "()Ljava/util/function/BiFunction;"
            )
        )
    {
        return true;
    }
    // The real KeyManagerFactory delegates to a provider SPI that cannot
    // materialize CratonVM's registry-backed JKS keys. The native bridge keeps
    // the per-entry password with the originating KeyStore and exposes a
    // functional X509KeyManager to SSLContext.init.
    if class_name == "javax/net/ssl/KeyManagerFactory"
        && matches!(
            (method_name, method_descriptor),
            ("init", "(Ljava/security/KeyStore;[C)V")
                | ("getKeyManagers", "()[Ljavax/net/ssl/KeyManager;")
        )
    {
        return true;
    }

    // File-attribute values are carried by a private five-slot synthetic
    // object, not by the real JDK's zero-field interface or platform-private
    // attribute layouts. Interface call sites must therefore dispatch to the
    // registered bridge before any receiver-class bytecode is selected.
    if class_name == "java/nio/file/attribute/BasicFileAttributes"
        && matches!(
            (method_name, method_descriptor),
            ("creationTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("lastAccessTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("lastModifiedTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("isDirectory", "()Z")
                | ("isRegularFile", "()Z")
                | ("isSymbolicLink", "()Z")
                | ("isOther", "()Z")
                | ("size", "()J")
                | ("fileKey", "()Ljava/lang/Object;")
        )
    {
        return true;
    }

    // Class loading is implemented by CratonVM's native bridge so that its
    // per-loader namespaces and parent-first delegation remain visible in
    // real-JDK mode. The JDK methods are concrete bytecode, so force the
    // bridge for inherited base calls (including invokespecial super calls
    // from custom loaders); direct subclass overrides remain selected by
    // their own declaring class.
    if class_name == "java/lang/ClassLoader"
        && method_name == "loadClass"
        && matches!(
            method_descriptor,
            "(Ljava/lang/String;)Ljava/lang/Class;" | "(Ljava/lang/String;Z)Ljava/lang/Class;"
        )
    {
        return true;
    }

    // The slow invoke path already forces these generic Class metadata
    // methods to their native Signature-attribute implementation. Keep the
    // warmed virtual-call cache in sync; otherwise a hot call bypasses the
    // override and re-enters the incomplete real-JDK reifier path.
    if class_name == "java/lang/Class"
        && matches!(
            (method_name, method_descriptor),
            ("getTypeParameters", "()[Ljava/lang/reflect/TypeVariable;")
                | ("getGenericInterfaces", "()[Ljava/lang/reflect/Type;")
                | ("getGenericSuperclass", "()Ljava/lang/reflect/Type;")
        )
    {
        return true;
    }

    // Real-JDK `Thread.run()` bytecode is layout-variant: older JDKs read
    // direct `Thread.target`, while newer layouts also carry the task in
    // `Thread$FieldHolder.task`. CratonVM's registered native mirrors VM
    // thread-start target resolution (direct field, holder task, synthetic
    // slot), so force it to win for normal and invokespecial `super.run()`
    // calls from Thread subclasses such as WildFly's JBossThread.
    if class_name == "java/lang/Thread" && method_name == "run" && method_descriptor == "()V" {
        return true;
    }

    // DELETED 2026-08-06 (JDK-ONLY-WAVE2, L11 item 7): the ninth,
    // receiver-blind `(ThreadPoolExecutor, execute, (Ljava/lang/Runnable;)V)`
    // arm, together with the eight receiver-shape probes that existed only to
    // undo it for a genuinely real receiver.
    //
    // It forced `native_es_execute` to win unconditionally because
    // `Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/
    // `newCachedThreadPool()` allocated their return value under the REAL class
    // name and did not run it through the real `<init>` -- so real `execute()`
    // bytecode read a null `ctl` and NPE'd
    // (threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md).
    //
    // Why it is gone, in the order the removal required:
    //
    //  1. L10 (2026-08-06): `NativeMethodRegistry::register` drops every
    //     `Executors` pool factory when `drop_real_layout_synthetic` is set, so
    //     the real `java.util.concurrent.Executors` bytecode builds every
    //     executor on a real-JDK image and no code path can mint a half-built
    //     one. The fabricated receiver this arm protected does not exist there.
    //  2. `native_es_execute` is tagged `NativeKind::SyntheticStub` and
    //     `java/util/concurrent/ThreadPoolExecutor` is on
    //     `real_protected_stub_class_common`'s allow-list, so the one
    //     centralised arbitration yields it to the real `execute()` body --
    //     class-scoped, for every receiver, on both the warm and the cold
    //     dispatch path. That is what keeps `ctx.invoke_virtual(pool,
    //     "execute", ...)` from recursing into the same native forever.
    //  3. Only then the nine sites.
    //
    // The native is NOT deleted, and must not be: strict mode declines to ADMIT
    // a native, it does not remove it, and the `--features synthetic-jdk` build
    // -- the only build where the real `ThreadPoolExecutor` bytecode this arm
    // overrode is absent -- still registers and still runs it.
    //
    // Do NOT re-add a name here without also re-adding the eight probes. This
    // arm has no receiver awareness and never had any.

    if class_name == "java/nio/ByteBuffer"
        && matches!(
            (method_name, method_descriptor),
            ("allocate", "(I)Ljava/nio/ByteBuffer;")
                | ("allocateDirect", "(I)Ljava/nio/ByteBuffer;")
                | ("wrap", "([B)Ljava/nio/ByteBuffer;")
                | ("wrap", "([BII)Ljava/nio/ByteBuffer;")
                | ("get", "()B")
                | ("get", "(I)B")
                | ("get", "([B)Ljava/nio/ByteBuffer;")
                | ("get", "([BII)Ljava/nio/ByteBuffer;")
                | ("put", "(B)Ljava/nio/ByteBuffer;")
                | ("put", "(IB)Ljava/nio/ByteBuffer;")
                | ("put", "([B)Ljava/nio/ByteBuffer;")
                | ("put", "([BII)Ljava/nio/ByteBuffer;")
                | ("put", "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;")
                | ("getShort", "()S")
                | ("getShort", "(I)S")
                | ("putShort", "(S)Ljava/nio/ByteBuffer;")
                | ("putShort", "(IS)Ljava/nio/ByteBuffer;")
                | ("getChar", "()C")
                | ("getChar", "(I)C")
                | ("putChar", "(C)Ljava/nio/ByteBuffer;")
                | ("putChar", "(IC)Ljava/nio/ByteBuffer;")
                | ("getInt", "()I")
                | ("getInt", "(I)I")
                | ("putInt", "(I)Ljava/nio/ByteBuffer;")
                | ("putInt", "(II)Ljava/nio/ByteBuffer;")
                | ("getLong", "()J")
                | ("getLong", "(I)J")
                | ("putLong", "(J)Ljava/nio/ByteBuffer;")
                | ("putLong", "(IJ)Ljava/nio/ByteBuffer;")
                | ("getFloat", "()F")
                | ("getFloat", "(I)F")
                | ("putFloat", "(F)Ljava/nio/ByteBuffer;")
                | ("getDouble", "()D")
                | ("putDouble", "(D)Ljava/nio/ByteBuffer;")
                | ("flip", "()Ljava/nio/Buffer;")
                | ("flip", "()Ljava/nio/ByteBuffer;")
                | ("clear", "()Ljava/nio/Buffer;")
                | ("clear", "()Ljava/nio/ByteBuffer;")
                | ("rewind", "()Ljava/nio/Buffer;")
                | ("rewind", "()Ljava/nio/ByteBuffer;")
                | ("mark", "()Ljava/nio/Buffer;")
                | ("mark", "()Ljava/nio/ByteBuffer;")
                | ("reset", "()Ljava/nio/Buffer;")
                | ("position", "()I")
                | ("position", "(I)Ljava/nio/Buffer;")
                | ("position", "(I)Ljava/nio/ByteBuffer;")
                | ("limit", "()I")
                | ("limit", "(I)Ljava/nio/Buffer;")
                | ("limit", "(I)Ljava/nio/ByteBuffer;")
                | ("capacity", "()I")
                | ("remaining", "()I")
                | ("hasRemaining", "()Z")
                | ("compact", "()Ljava/nio/ByteBuffer;")
                | ("array", "()[B")
                | ("arrayOffset", "()I")
                | ("hasArray", "()Z")
                | ("isDirect", "()Z")
                | ("isReadOnly", "()Z")
                | ("order", "()Ljava/nio/ByteOrder;")
                | ("order", "(Ljava/nio/ByteOrder;)Ljava/nio/ByteBuffer;")
                | ("slice", "()Ljava/nio/ByteBuffer;")
                | ("duplicate", "()Ljava/nio/ByteBuffer;")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("hashCode", "()I")
                | ("compareTo", "(Ljava/nio/ByteBuffer;)I")
                | ("toString", "()Ljava/lang/String;")
        )
    {
        return true;
    }

    // Bulk `get(T[],int,int)`/`put(T[],int,int)` on every typed NIO buffer
    // (Int/Long/Short/Float/DoubleBuffer) are CONCRETE (not abstract) real
    // JDK 25 bytecode — `FloatBuffer.getArray`/`putArray` etc. read/write
    // via `this.address` + `ScopedMemoryAccess` directly for any length
    // beyond a trivial few elements, bypassing virtual dispatch to the
    // single-element accessors entirely. Our synthetic abstract-stamped
    // typed-buffer views (`native-builtins/src/servlet.rs`'s
    // `s2_typed_buffer_view_fns!`, produced by e.g.
    // `ByteBuffer.asFloatBuffer()`) never set a real `address` field, so
    // that fast path silently read/wrote zero bytes for every bulk vector
    // transfer — the dominant access pattern for ES/Lucene vector codecs
    // (`buffer.get(vec, 0, dims)`), surfacing as
    // "expected:<X> but was:<0.0>" across nearly the whole ES vector-codec
    // test family. Registering the natives (in servlet.rs) is not enough by
    // itself since real bytecode already exists for these signatures; force
    // it to win here, mirroring the ByteBuffer block above.
    if matches!(
        class_name,
        "java/nio/IntBuffer"
            | "java/nio/LongBuffer"
            | "java/nio/ShortBuffer"
            | "java/nio/FloatBuffer"
            | "java/nio/DoubleBuffer"
    ) && matches!(
        (method_name, method_descriptor),
        ("get", "([III)Ljava/nio/IntBuffer;")
            | ("put", "([III)Ljava/nio/IntBuffer;")
            | ("get", "([JII)Ljava/nio/LongBuffer;")
            | ("put", "([JII)Ljava/nio/LongBuffer;")
            | ("get", "([SII)Ljava/nio/ShortBuffer;")
            | ("put", "([SII)Ljava/nio/ShortBuffer;")
            | ("get", "([FII)Ljava/nio/FloatBuffer;")
            | ("put", "([FII)Ljava/nio/FloatBuffer;")
            | ("get", "([DII)Ljava/nio/DoubleBuffer;")
            | ("put", "([DII)Ljava/nio/DoubleBuffer;")
    ) {
        return true;
    }

    if class_name == "java/util/concurrent/LinkedBlockingDeque"
        && method_name == "clear"
        && method_descriptor == "()V"
    {
        return true;
    }

    if class_name == "jdk/internal/util/ArraysSupport"
        && matches!(
            (method_name, method_descriptor),
            ("vectorizedHashCode", "(Ljava/lang/Object;IIII)I")
                | (
                    "vectorizedMismatch",
                    "(Ljava/lang/Object;JLjava/lang/Object;JII)I"
                )
                | ("mismatch", "([B[BI)I")
                | ("mismatch", "([BI[BII)I")
                | ("mismatch", "([C[CI)I")
                | ("mismatch", "([CI[CII)I")
        )
    {
        return true;
    }

    if class_name == "java/io/FilterInputStream"
        && matches!(
            (method_name, method_descriptor),
            ("<init>", "(Ljava/io/InputStream;)V") | ("skip", "(J)J")
        )
    {
        return true;
    }

    if class_name == "java/io/ByteArrayInputStream"
        && matches!(
            (method_name, method_descriptor),
            ("read", "()I")
                | ("read", "([BII)I")
                | ("available", "()I")
                | ("skip", "(J)J")
                | ("close", "()V")
        )
    {
        return true;
    }

    if (matches!(
        class_name,
        "java/lang/Iterable" | "java/util/Collection" | "java/util/Set" | "java/util/EnumSet"
    ) && method_name == "iterator"
        && method_descriptor == "()Ljava/util/Iterator;")
    {
        return true;
    }

    // Map.forEach is a default method whose JDK implementation iterates an
    // entrySet. CratonVM's immutable-map wrapper intentionally stores a
    // snapshot backing rather than the JDK's MapN layout, so running that body
    // can materialize a HashSet and hash a cyclic map entry before a caller's
    // own nesting guard runs. The native bridge snapshots concrete map entries
    // directly and preserves the Map.forEach contract for every map backend.
    if class_name == "java/util/Map"
        && method_name == "forEach"
        && method_descriptor == "(Ljava/util/function/BiConsumer;)V"
    {
        return true;
    }

    if class_name == "java/util/Iterator" && matches!(method_name, "hasNext" | "next" | "remove") {
        return true;
    }

    if class_name == "java/lang/Thread"
        && method_name == "getThreadGroup"
        && method_descriptor == "()Ljava/lang/ThreadGroup;"
    {
        return true;
    }

    if class_name == "org/jboss/threads/JBossThread"
        && method_name == "run"
        && method_descriptor == "()V"
    {
        return true;
    }

    if class_name == "org/jboss/threads/JBossThread"
        && method_name == "onExit"
        && method_descriptor == "(Ljava/lang/Runnable;)Z"
    {
        return true;
    }

    if class_name == "org/jboss/threads/JBossThreadFactory"
        && ((method_name == "newThread"
            && method_descriptor == "(Ljava/lang/Runnable;)Ljava/lang/Thread;")
            || (method_name == "access$100"
                && method_descriptor
                    == "(Lorg/jboss/threads/JBossThreadFactory;Ljava/lang/Runnable;)Ljava/lang/Thread;"))
    {
        return true;
    }

    if class_name == "java/io/InputStreamReader"
        && method_name == "close"
        && method_descriptor == "()V"
    {
        return true;
    }

    if class_name == "java/lang/SecurityManager"
        && method_name == "getRootGroup"
        && method_descriptor == "()Ljava/lang/ThreadGroup;"
    {
        return true;
    }

    if class_name == "java/util/AbstractSet"
        && method_name == "hashCode"
        && method_descriptor == "()I"
    {
        return true;
    }

    if class_name == "java/util/AbstractCollection"
        && method_name == "contains"
        && method_descriptor == "(Ljava/lang/Object;)Z"
    {
        return true;
    }

    if class_name == "java/lang/Class"
        && (method_name == "getEnumConstants" || method_name == "getEnumConstantsShared")
        && method_descriptor == "()[Ljava/lang/Object;"
    {
        return true;
    }

    // `java.util.logging.Level.parse(String)` real bytecode resolves custom
    // and even standard level names through `KnownLevel.findByName`, which
    // on JDK 25 throws internally (a `Module`-null NPE the method's own
    // catch-all reports as a generic `IllegalArgumentException: Bad level`)
    // — see `kc16-blocker-map.md`'s KC16 investigation.
    // This broke WildFly's own `host.xml`/`domain.xml` parsing of
    // `<level name="WARN"/>` (org.jboss.logmanager's extended levels) before
    // it ever reached a genuinely-unknown name. Force the registered native
    // (`native_level_parse`, native-builtins/src/logmanager.rs), which
    // answers from the standard + JBoss LogManager static Level constants
    // directly, bypassing the broken registry lookup.
    if class_name == "java/util/logging/Level"
        && method_name == "parse"
        && method_descriptor == "(Ljava/lang/String;)Ljava/util/logging/Level;"
    {
        return true;
    }

    if is_forkjoin_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_aqls_state_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_bc_crypto_math_native_override(class_name, method_name, method_descriptor) {
        return true;
    }

    // The JDK's final owner setter is the single authoritative transition for
    // AbstractQueuedSynchronizer-derived locks. Route it through the native
    // registry so ThreadMXBean can retain an exact, moving-GC-safe index of
    // ownable synchronizers even after this tiny method has been JIT compiled.
    if class_name == "java/util/concurrent/locks/AbstractOwnableSynchronizer"
        && method_name == "setExclusiveOwnerThread"
        && method_descriptor == "(Ljava/lang/Thread;)V"
    {
        return true;
    }

    // java.lang.Module access checks. CratonVM's `Class.getModule()` returns a
    // synthetic Module mirror with a NULL `descriptor` (real module-path
    // encapsulation does not exist — every class is effectively on the class
    // path). The real `Module.isExported`/`isOpen` bytecode dereferences
    // `this.descriptor.isOpen()` inside `implIsExportedOrOpen` and NPEs (e.g.
    // Groovy `CachedClass.getMethods` → `checkCanSetAccessible`, Hibernate's
    // `JdbcTypeNameMapper.<clinit>` reflecting over `java.sql.Types`). Force the
    // registry-backed natives registered in `native-builtins` so the access check
    // is answered from the boot `ModuleRegistry`'s accurate per-module
    // exports/opens (java.base exports `java.lang`/… to all but not
    // `jdk.internal.*` — which ByteBuddy's `JavaDispatcher` relies on) instead of
    // touching the null descriptor.
    //
    // ClassLoader resource methods have the same issue: real JDK bytecode
    // walks URLClassPath state which CratonVM intentionally replaces with
    // native per-loader lookups.  Keep the singular, stream, and bulk
    // methods together so URLClassLoader instances do not fall back to the
    // process-wide dynamic classpath (which leaks resources between test
    // loaders) and null arguments retain their specified NPE contract.
    if class_name == "java/lang/ClassLoader"
        && matches!(
            method_name,
            "getResource"
                | "getSystemResource"
                | "getResources"
                | "getSystemResources"
                | "getResourceAsStream"
                | "getSystemResourceAsStream"
        )
    {
        return true;
    }

    // `java.net.URLClassLoader` declares its OWN `getResourceAsStream`
    // override (unlike `getResource`/`getResources`/`findResource`, which it
    // leaves to `ClassLoader`/its own `findResource` extension point) — real
    // OpenJDK wraps the stream so it can be tracked in the `closeables`
    // WeakHashMap for `close()`. That means the check above, keyed on
    // declaring class `java/lang/ClassLoader`, never matches a plain
    // `URLClassLoader` (or subclass that doesn't itself override
    // `getResourceAsStream`) instance's call — its declaring class resolves
    // to `java/net/URLClassLoader` instead, so real bytecode ran unforced.
    // That bytecode still depends on the same unpopulated `ucp`
    // (`URLClassPath`) internals the comment above describes, but ALSO
    // doesn't do the parent-delegation the native bridge implements: a
    // `new URLClassLoader(urls, parent)` whose only own URL is e.g. a
    // `@TempDir` holding a generated `META-INF/spring.components` index
    // (Spring Boot's `ServletComponentScanIntegrationTests
    // .indexedComponentsAreRegistered`) found the index fine via
    // `getResource`/`findResource` (both correctly native-forced already)
    // but got `null` from `getResourceAsStream` for every `.class` resource
    // that only the PARENT classloader's classpath actually holds —
    // `ClassPathResource.getInputStream()` then threw `FileNotFoundException`
    // reading an indexed component class that plainly exists. Force native
    // dispatch here too so `URLClassLoader.getResourceAsStream` resolves via
    // the same delegation-aware bridge (`classloader::cl_get_resource_as_stream`)
    // as the base-class methods above.
    if class_name == "java/net/URLClassLoader" && method_name == "getResourceAsStream" {
        return true;
    }

    // `java.nio.file.Path` is a genuine interface with no `toString()` body of
    // its own (nor `equals`/`hashCode`, but those aren't implicated here) —
    // real method resolution for `someSyntheticPathObj.toString()` walks up to
    // `java.lang.Object`, the only class in the chain that actually declares
    // `toString()` with a Code attribute. Without an entry here keyed on
    // `java/nio/file/Path` itself, that resolved declaring class
    // (`java/lang/Object`) is what gets checked against this gate — never
    // matches — so real `Object.toString()` runs (`getClass().getName() + "@"
    // + hashCode`) instead of the registered native
    // (`native-builtins::phases_late::register_phase57_nio_file`'s
    // `Path.toString()`, which correctly renders the jar-FS/host path).
    // `redefine_immune_path_native` below already anticipated this exact
    // (class, method) pair for the Mockito-redefine-immunity check, but the
    // actual force-native entry that makes it relevant was never added —
    // this closes that gap. Concretely this broke real javac's in-process
    // `JavacFileManager.inferBinaryName` for every `PathFileObject$JarFileObject`
    // classpath entry: its native fast path (`native_javac_file_manager_infer_binary_name`)
    // calls `path.toString()` expecting the in-jar relative path (e.g.
    // `/org/springframework/beans/factory/config/BeanDefinition.class`) but
    // got the garbage `Object.toString()` form (`java.nio.file.Path@1a2b3c`)
    // instead, which `javac_binary_name_from_relative_path` then mangled into
    // the literal binary name `java.nio.file` for EVERY application-classpath
    // class file — so `TestCompiler`/any real in-process `javac` compile of
    // source referencing an ordinary (non-JRT) classpath class failed with
    // "cannot find symbol", even for basic classes like
    // `org.springframework.beans.factory.support.RootBeanDefinition`
    // (`ServletComponentScanRegistrarTests
    // #processAheadOfTimeDoesNotRegisterServletComponentRegisteringPostProcessor`).
    if class_name == "java/nio/file/Path"
        && method_name == "toString"
        && method_descriptor == "()Ljava/lang/String;"
    {
        return true;
    }
    // Spring Boot's nested archive protocol reaches the registered jar-FS
    // bridge through these concrete real-JDK entry points. Letting the real
    // bodies win discards the `jar:nested:` container identity before the
    // native virtual filesystem can decode it.
    if (class_name == "java/nio/file/Path"
        && method_name == "of"
        && method_descriptor == "(Ljava/net/URI;)Ljava/nio/file/Path;")
        || (class_name == "java/nio/file/FileSystems" && method_name == "newFileSystem")
        || (class_name == "java/nio/file/spi/FileSystemProvider" && method_name == "newFileSystem")
        || (class_name == "org/springframework/boot/loader/launch/Archive"
            && method_name == "create"
            && matches!(
                method_descriptor,
                "(Ljava/io/File;)Lorg/springframework/boot/loader/launch/Archive;"
                    | "(Ljava/lang/Class;)Lorg/springframework/boot/loader/launch/Archive;"
            ))
    {
        return true;
    }

    // `getDescriptor` has the same null-descriptor problem, but real HotSpot
    // guarantees `isNamed() == (getDescriptor() != null)` — a named module's
    // descriptor is never null. CratonVM's `isNamed()` (real bytecode, reading
    // the dual-written real `name` field) can report a classpath-loaded,
    // modularized jar as named (see `classloading::module::ModuleDescriptor
    // ::automatic`), yet `getDescriptor()`'s real bytecode (`return this
    // .descriptor;`) reads a field CratonVM never populates. Any code that
    // only calls `getDescriptor()` after checking `isNamed()` (e.g.
    // Elasticsearch's `ProviderLocator.checkUses` — `caller.isNamed() &&
    // caller.getDescriptor().uses()...`) gets `NullPointerException: Cannot
    // invoke "ModuleDescriptor.uses()" because the return value of
    // "Module.getDescriptor()" is null`, breaking `XContentProvider$Holder`
    // static init and cascading into thousands of Elasticsearch suite
    // failures via `NoClassDefFoundError`. Force the native (registered in
    // `native-builtins::lib::register_essential_natives`, alongside
    // isExported/isOpen above), which returns null only for the true unnamed
    // module and otherwise builds a descriptor backed by the boot
    // `ModuleRegistry`'s parsed `uses`.
    // `canUse`/`addUses` have the SAME null-descriptor problem as
    // `getDescriptor` above: their real bytecode reads `this.descriptor`
    // directly (`return descriptor.isAutomatic() || descriptor.uses()
    // .contains(sn);` for `canUse`; a similar direct field read for
    // `addUses`) rather than going through the `getDescriptor()` accessor,
    // so forcing `getDescriptor` alone does not protect them. A named
    // Module mirror (`isNamed()` true) whose `descriptor` field is unset
    // NPEs the moment either method runs -- observed via WildFly Host
    // Controller's parallel extension loader (`DeferredExtensionContext
    // .load()`): loading `org.jboss.as.jmx` (which depends on the real
    // platform module `java.management`) reaches JDK-internal module
    // helper code that calls `Module.canUse`/`addUses` on a Module the VM
    // handed out without a populated descriptor, surfacing as
    // `NullPointerException: Cannot invoke "ModuleDescriptor.isAutomatic()"
    // because "this.descriptor" is null` wrapped in an `ExecutionException`
    // from the extension loader's `Future.get()`, which
    // `ControllerLogger.failedToLoadModule` re-reports as `WFLYCTL0083:
    // Failed to load module org.jboss.as.jmx`. `canUse` already had a
    // registered native (S109 Wave3, `native-builtins::lib`) that was never
    // added here, so it was silently shadowed by the real bytecode in
    // real-JDK mode; `addUses` had no native at all until this fix.
    if class_name == "java/lang/Module"
        && matches!(
            method_name,
            "isExported"
                | "isOpen"
                | "getDescriptor"
                | "canUse"
                | "addUses"
                // `getResourceAsStream` has no real-JDK-viable body here: it
                // routes through `BuiltinClassLoader.findResourceAsStream` /
                // a `ModuleReader`, neither of which CratonVM models. The
                // native lives in `jboss_jdkspecific::native_module_get_resource_as_stream`.
                | "getResourceAsStream"
                | "addExports"
                | "addOpens"
                | "implAddExports"
                | "implAddExportsToAllUnnamed"
                | "implAddExportsNoSync"
                | "implAddOpens"
                | "implAddOpensToAllUnnamed"
        )
    {
        return true;
    }
    // JavaLangAccess is implemented by the concrete System$1 singleton. The
    // JDK module bootstrap calls these ordinary Java methods through that
    // receiver, so native registrations must win over its real bytecode.
    if matches!(class_name, "java/lang/System$1" | "java/lang/System$2")
        && matches!(
            (method_name, method_descriptor),
            ("addReads", "(Ljava/lang/Module;Ljava/lang/Module;)V")
                | ("addReadsAllUnnamed", "(Ljava/lang/Module;)V")
                | ("addExports", "(Ljava/lang/Module;Ljava/lang/String;)V")
                | (
                    "addExports",
                    "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V"
                )
                | (
                    "addExportsToAllUnnamed",
                    "(Ljava/lang/Module;Ljava/lang/String;)V"
                )
                | (
                    "addOpens",
                    "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V"
                )
                | (
                    "addOpensToAllUnnamed",
                    "(Ljava/lang/Module;Ljava/lang/String;)V"
                )
                | ("addUses", "(Ljava/lang/Module;Ljava/lang/Class;)V")
        )
    {
        return true;
    }
    // `ServerSocket.getLocalSocketAddress()` is pure Java in the real JDK:
    // it calls `getInetAddress()` and then constructs an InetSocketAddress.
    // CratonVM's real ServerSocket instances carry their live listener state
    // in native side-tables / synthetic slots, while some real SocketImpl
    // fields remain unpopulated. Force the registered natives for these
    // accessors so WildFly's process controller sees a resolved bound address
    // instead of `/0.0.0.0:PORT` with a null InetAddress.
    if class_name == "java/net/ServerSocket"
        && matches!(
            (method_name, method_descriptor),
            ("getInetAddress", "()Ljava/net/InetAddress;")
                | ("getLocalSocketAddress", "()Ljava/net/SocketAddress;")
        )
    {
        return true;
    }
    // `java.util.jar.JarFile` has real JDK bytecode backed by native ZipFile
    // state and fields (`manRef`, `jv`, etc.) CratonVM does not initialize.
    // The native-builtins JarFile bridge stores path/manifest in its compact
    // synthetic layout and reads ZIP data with Rust's zip crate, so it must win
    // for both interpreted and shared exec dispatch. Keep this in sync with
    // the JarFile gate in vm_exec.rs.
    if class_name == "java/util/jar/JarFile"
        && matches!(
            method_name,
            "<init>"
                | "getManifest"
                | "getManifestFromReference"
                | "stream"
                | "entries"
                | "getEntry"
                | "getJarEntry"
                | "getInputStream"
                | "size"
                | "close"
                | "getName"
        )
    {
        return true;
    }
    // Manifest attributes are keyed by Attributes.Name, whose equality and
    // hash are case-insensitive. The bridge stores those names in the native
    // HashMap path, so its registered Name methods must win over real JDK
    // bytecode (which expects uninitialized private cache fields).
    if class_name == "java/util/jar/Attributes$Name"
        && matches!(
            (method_name, method_descriptor),
            ("<init>", "(Ljava/lang/String;)V")
                | ("toString", "()Ljava/lang/String;")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("hashCode", "()I")
        )
    {
        return true;
    }
    if class_name == "java/util/jar/Attributes"
        && method_name == "containsKey"
        && method_descriptor == "(Ljava/lang/Object;)Z"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/jar/ManifestInfo"
        && method_name == "isMultiRelease"
        && method_descriptor == "()Z"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/jar/NestedJarFile"
        && method_name == "getJarEntry"
        && method_descriptor == "(Ljava/lang/String;)Ljava/util/jar/JarEntry;"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/jar/NestedJarFile$NestedJarEntry"
        && method_name == "getRealName"
        && method_descriptor == "()Ljava/lang/String;"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/net/protocol/jar/UrlJarFile"
        && method_name == "getEntry"
        && method_descriptor == "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/zip/ZipContent$SignatureFiles"
        && matches!(
            (method_name, method_descriptor),
            ("<clinit>", "()V") | ("bufferEndsWithSignatureSuffix", "()Z")
        )
    {
        return true;
    }
    // `java.util.jar.Manifest` constructors/accessors are small but depend on
    // real-JDK stream/parser state that is fragile for synthetic jarfs streams.
    // Force the bridge parser so `EmbeddedModulePath.moduleNameFromManifestOrNull`
    // sees a real, non-null Attributes object.
    if class_name == "java/util/jar/Manifest"
        && matches!(
            (method_name, method_descriptor),
            ("<init>", "()V")
                | ("<init>", "(Ljava/io/InputStream;)V")
                | ("<init>", "(Ljava/io/InputStream;Ljava/lang/String;)V")
                | ("<init>", "(Ljava/util/jar/Manifest;)V")
                | (
                    "<init>",
                    "(Ljava/util/jar/JarVerifier;Ljava/io/InputStream;Ljava/lang/String;)V"
                )
                | ("getMainAttributes", "()Ljava/util/jar/Attributes;")
                | ("getEntries", "()Ljava/util/Map;")
        )
    {
        return true;
    }
    // MethodHandles VarHandle factories must return CratonVM synthetic handles
    // carrying native side-table/layout metadata. The real JDK bytecode creates
    // private VarHandle subclasses whose layouts our native get/set paths cannot
    // decode, so byte-array views read back null/zero.
    if is_method_handles_varhandle_factory_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    // FFM layout factories: THE SECOND COPY OF THIS TABLE IS GONE (F27,
    // 2026-08-13). What stood here was an inline `matches!` over the same
    // `java/lang/foreign/MemoryLayout` triples that
    // `is_ffm_memory_layout_native_override` lists — nine of them verbatim,
    // inside the SAME function that calls that helper a few hundred lines below.
    // Both arms returned `true`, so the duplication was invisible; it was also
    // the only reason the four stale erased-return descriptors had to be deleted
    // twice. The helper is a strict superset (it adds `name` and `withName`), so
    // deleting this block changes no triple's answer.
    //
    // The rationale it carried, kept because it is the reason the routing exists
    // at all: JDK 25's real `MemoryLayout.sequenceLayout` runs through
    // `jdk/internal/foreign/Utils` while `SharedUtils.<clinit>` is still building
    // its `C_POINTER` constant. That circular path re-enters `SharedUtils` before
    // `ValueLayout.JAVA_BYTE` has been populated and
    // `Objects.requireNonNull(elementLayout)` throws a bare NPE. The registered
    // native factories are bytecode-equivalent for CratonVM's supported Panama
    // layout model and avoid that bootstrap cycle.

    // FFM ValueLayout subinterfaces are abstract/covariant in the real JDK
    // surface. CratonVM backs the supported layouts with small synthetic
    // objects, so calls such as `ValueLayout$OfFloat.withByteAlignment(J)`
    // must be served by the registered layout shims instead of falling through
    // to an abstract interface method with no Code attribute.
    if (class_name == "java/lang/foreign/ValueLayout"
        || class_name == "java/lang/foreign/AddressLayout"
        || class_name.starts_with("java/lang/foreign/ValueLayout$")
        || class_name.starts_with("jdk/internal/foreign/layout/ValueLayouts$"))
        && matches!(
            method_name,
            "byteSize"
                | "byteAlignment"
                | "withByteAlignment"
                | "withName"
                | "withOrder"
                | "varHandle"
                | "name"
                | "carrier"
                | "order"
                | "targetLayout"
                | "withTargetLayout"
        )
    {
        return true;
    }
    if class_name == "java/lang/foreign/MemorySegment"
        && matches!(
            method_name,
            "byteSize"
                | "address"
                | "copy"
                | "get"
                | "set"
                | "getAtIndex"
                | "setAtIndex"
                | "asSlice"
                | "isNative"
                | "isMapped"
                | "isReadOnly"
                | "scope"
                | "ofArray"
        )
    {
        return true;
    }
    if matches!(
        class_name,
        "jdk/internal/foreign/AbstractMemorySegmentImpl"
            | "jdk/internal/foreign/NativeMemorySegmentImpl"
            | "jdk/internal/foreign/MappedMemorySegmentImpl"
    ) && matches!(
        method_name,
        "byteSize" | "address" | "get" | "isNative" | "isMapped" | "isReadOnly" | "scope"
    ) {
        return true;
    }
    if class_name == "jdk/internal/foreign/MemorySessionImpl"
        && matches!(
            method_name,
            "toMemorySession"
                | "createConfined"
                | "createShared"
                | "createImplicit"
                | "createHeap"
                | "addCloseAction"
                | "addOrCleanupIfFail"
                | "addInternal"
                | "release0"
                | "acquire0"
                | "whileAlive"
                | "ownerThread"
                | "isAccessibleBy"
                | "isAlive"
                | "checkValidStateRaw"
                | "checkValidState"
                | "isCloseable"
                | "close"
                | "justClose"
        )
    {
        return true;
    }
    if class_name == "jdk/internal/misc/ScopedMemoryAccess"
        && matches!(
            method_name,
            "getByte"
                | "getByteInternal"
                | "putByte"
                | "putByteInternal"
                | "getShort"
                | "getShortInternal"
                | "getShortUnaligned"
                | "getShortUnalignedInternal"
                | "putShort"
                | "putShortInternal"
                | "putShortUnaligned"
                | "putShortUnalignedInternal"
                | "getInt"
                | "getIntInternal"
                | "getIntUnaligned"
                | "getIntUnalignedInternal"
                | "putInt"
                | "putIntInternal"
                | "putIntUnaligned"
                | "putIntUnalignedInternal"
                | "getLong"
                | "getLongInternal"
                | "getLongUnaligned"
                | "getLongUnalignedInternal"
                | "putLong"
                | "putLongInternal"
                | "putLongUnaligned"
                | "putLongUnalignedInternal"
                | "copyMemory"
                | "copyMemoryInternal"
        )
    {
        return true;
    }
    // ByteArrayOutputStream is frequently subclassed by JDK internals. The VM
    // already forces these intrinsics in the slow shared-invocation path; keep
    // the interpreter cache gate in sync so ordinary bytecode dispatch also
    // uses the registered native overloads, including charset-aware toString.
    if class_name == "java/io/ByteArrayOutputStream"
        && matches!(
            method_name,
            "write" | "toByteArray" | "size" | "reset" | "toString"
        )
    {
        return true;
    }
    // The lightweight resource-reader bridge stores the backing InputStream in
    // the reader slot used by the native read shim. Real JDK close() expects a
    // fully initialized sun.nio.cs.StreamDecoder in `sd` and can NPE while
    // closing META-INF/services readers during Elasticsearch provider loading.
    if class_name == "java/io/InputStreamReader"
        && matches!((method_name, method_descriptor), ("close", "()V"))
    {
        return true;
    }
    if (class_name == "java/lang/Runtime"
        && method_name == "version"
        && method_descriptor == "()Ljava/lang/Runtime$Version;")
        || (class_name == "java/lang/Runtime$Version"
            && matches!(
                (method_name, method_descriptor),
                ("feature", "()I") | ("build", "()Ljava/util/Optional;")
            ))
    {
        return true;
    }
    if is_spring_mock_response_native_override(class_name, method_name, method_descriptor)
        || is_script_engine_manager_native_override(class_name, method_name, method_descriptor)
        || is_jython_thread_state_native_override(class_name, method_name, method_descriptor)
        || is_jython_pyobject_native_override(class_name, method_name, method_descriptor)
        || is_jython_imp_native_override(class_name, method_name, method_descriptor)
        || is_jython_pymodule_native_override(class_name, method_name, method_descriptor)
    {
        return true;
    }

    if class_name == "java/nio/charset/Charset"
        && ((method_name == "availableCharsets" && method_descriptor == "()Ljava/util/SortedMap;")
            || (method_name == "aliases" && method_descriptor == "()Ljava/util/Set;"))
    {
        return true;
    }

    // JBoss LogManager fallback. CratonVM often creates synthetic
    // `org.jboss.logmanager.Logger` instances without a real `LoggerNode` graph.
    // The native-builtins logmanager shim already registers null-safe
    // `getEffectiveLevel()I` and `isLoggable(Level)` natives, but the real
    // jboss-logmanager bytecode dereferences `this.loggerNode` first. Force the
    // natives for real-JDK class bodies too, matching the existing null-safe
    // logRaw / handler overrides in `native-builtins::logmanager`.
    if (class_name == "org/jboss/logmanager/Logger" || class_name == "org.jboss.logmanager.Logger")
        && matches!(
            (method_name, method_descriptor),
            ("getEffectiveLevel", "()I") | ("isLoggable", "(Ljava/util/logging/Level;)Z")
        )
    {
        return true;
    }

    // JBoss Modules asks Module.forClass(caller) to locate the caller's
    // org.jboss.modules.Module before service-loading extension modules.
    // CratonVM tracks java.lang.Module mirrors there instead, so the real
    // bytecode can throw a bare ModuleLoadException for valid WildFly modules.
    // Force the native bridge that loads through the synthetic boot loader.
    if class_name == "org/jboss/modules/Module"
        && method_name == "loadServiceFromCallerModuleLoader"
        && matches!(
            method_descriptor,
            "(Ljava/lang/String;Ljava/lang/Class;)Ljava/util/ServiceLoader;"
                | "(Lorg/jboss/modules/ModuleIdentifier;Ljava/lang/Class;)Ljava/util/ServiceLoader;"
        )
    {
        return true;
    }

    // `Module.loadService(Class)` (the instance method, distinct from the
    // static bridge above) walks `getClass().getModule().addUses(...)` in
    // real jboss-modules bytecode before ever reading `moduleClassLoader` —
    // a JDK-module-system bookkeeping call CratonVM's permissive module
    // model doesn't need. Force the native reimplementation that skips
    // straight to `ServiceLoader.load(serviceType, moduleClassLoader)`.
    if class_name == "org/jboss/modules/Module"
        && method_name == "loadService"
        && method_descriptor == "(Ljava/lang/Class;)Ljava/util/ServiceLoader;"
    {
        return true;
    }

    // `ModuleClassLoader.getResources`/`findResources` (and the singular
    // `getResource`/`findResource`): our synthetic ModuleClassLoader
    // instances are allocated via `alloc_concurrent_synthetic`, bypassing
    // the real constructor, so real bytecode's internal `ResourceLoader`
    // state is never populated and these methods silently return empty
    // results instead of the module's own resources (notably
    // `META-INF/services/*`, which `ServiceLoader.load` needs — e.g. WildFly
    // extension modules like `org.jboss.as.jmx` register their `Extension`
    // provider there). Force the registered natives that walk the module's
    // resolved resource roots directly instead.
    if class_name == "org/jboss/modules/ModuleClassLoader"
        && matches!(
            method_name,
            "findClass" | "getResources" | "findResources" | "getResource" | "findResource"
        )
    {
        return true;
    }

    // Spring RSocket async setup can encode data and metadata strings on two
    // Reactor workers at the same time. The real `CharSequenceEncoder` lazily
    // computes a charset capacity through a per-instance cache; under CratonVM
    // that cold concurrent path can strand one worker before the setup payload
    // zip completes. Force the conservative native capacity helper registered in
    // `native-builtins` so the normal Spring `DataBuffer.write` still performs
    // the actual encoding, but the fragile lazy cache path is bypassed.
    if class_name == "org/springframework/core/codec/CharSequenceEncoder"
        && method_name == "calculateCapacity"
        && method_descriptor == "(Ljava/lang/CharSequence;Ljava/nio/charset/Charset;)I"
    {
        return true;
    }
    // BUG-15: `sun.util.locale.provider.LocaleResources.getDateTimePattern(int,
    // int, Calendar)` reads its pattern arrays through `LocaleData
    // .getDateFormatData` → `Bundles.of(...)`, the jdk.localedata class-based
    // resource path CratonVM does not surface, so it returns a NULL pattern.
    // `DateFormatProviderImpl.getInstance` then builds `new SimpleDateFormat(
    // null, locale)` → `compile(null)` → NPE ("pattern is null"), breaking
    // MessageFormat `{n,date}`/`{n,time}` elements and the
    // `DateFormat.get{Date,Time}Instance` factories. Force our native (returns
    // the en/de CLDR pattern directly) so the downstream real-JDK
    // SimpleDateFormat runs with a valid pattern. Companion native registered in
    // `native-builtins::locale_resources::register`; same locale-data-gap class
    // as the BreakIterator / getDecimalFormatSymbolsData overrides.
    // The java.time localized-formatting path
    // (DateTimeFormatterBuilder.getLocalizedDateTimePattern →
    // getJavaTimeDateTimePattern) reads the same unsurfaced jdk.localedata
    // bundle and otherwise returns null → `appendPattern(null)` NPE ("pattern"),
    // breaking Spring's LocalDate/LocalDateTime style formatting & parsing.
    if class_name == "sun/util/locale/provider/LocaleResources"
        && (method_name == "getDateTimePattern" || method_name == "getJavaTimeDateTimePattern")
    {
        return true;
    }
    // java.time text names: `sun.util.locale.provider.CalendarDataUtility
    // .retrieveJavaTimeFieldValueName(s)` feed `DateTimeTextProvider`'s
    // `EEE`/`MMM`/`a`/`G` lookups. The real-JDK bodies walk the same
    // `jdk.localedata` CLDR bundles we don't surface (as getDateTimePattern
    // above) and return null/empty, so `DateTimeFormatter` prints the raw
    // numeric field (e.g. Spring `HttpHeaders` RFC-1123 dates render
    // "4, 18 12 2008" not "Thu, 18 Dec 2008"). Force our natives (registered
    // in `native-builtins::locale_resources::register`) which answer from the
    // en/US CLDR name tables directly.
    if class_name == "sun/util/locale/provider/CalendarDataUtility"
        && matches!(
            method_name,
            "retrieveJavaTimeFieldValueName" | "retrieveJavaTimeFieldValueNames"
        )
    {
        return true;
    }
    // Unicode normalization: `java.text.Normalizer.normalize/isNormalized` — the
    // real-JDK bodies drive `sun.text.normalizer` off ICU normalization data
    // (`jdk.localedata`-adjacent tables) that CratonVM doesn't surface, so they
    // return garbage (e.g. `normalize("ï", NFD)` yields six U+0226 chars).
    // Force our natives (registered in `register_p61_text_formatting`), which
    // use the `unicode-normalization` crate for faithful NFC/NFD/NFKC/NFKD. Fixes
    // Spring `ContentDisposition.transliterateToAscii` (accent decomposition).
    if class_name == "java/text/Normalizer" && matches!(method_name, "normalize" | "isNormalized") {
        return true;
    }
    if is_awt_imageio_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // javac calls this helper while scanning standard file-manager locations.
    // The JDK 25 body is a one-token regex (`\\bMODULE\\b`). Letting that real
    // regex bytecode run under CratonVM can stall in Pattern$Bound/CharPredicates
    // during Spring's in-memory compilation tests; the registered native answers
    // the bytecode-equivalent boolean directly.
    if class_name == "javax/tools/StandardLocation"
        && method_name == "computeIsModuleOrientedLocation"
        && method_descriptor == "(Ljava/lang/String;)Z"
    {
        return true;
    }

    // Same javac location hot path as above: once `inferBinaryName` delegates to
    // `JavacFileManager`, this guard can be reached for every scanned classfile.
    // The native preserves the module-oriented rejection while avoiding repeated
    // interpreted interface/default-method dispatch in the compiler loop.
    if class_name == "com/sun/tools/javac/file/JavacFileManager"
        && matches!(
            (method_name, method_descriptor),
            ("checkNotModuleOrientedLocation", "(Ljavax/tools/JavaFileManager$Location;)V")
                | (
                    "list",
                    "(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;Ljava/util/Set;Z)Ljava/lang/Iterable;"
                )
                | (
                    "inferBinaryName",
                    "(Ljavax/tools/JavaFileManager$Location;Ljavax/tools/JavaFileObject;)Ljava/lang/String;"
                )
        )
    {
        return true;
    }

    if class_name == "com/sun/tools/javac/file/RelativePath"
        && matches!(
            (method_name, method_descriptor),
            ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("compareTo", "(Lcom/sun/tools/javac/file/RelativePath;)I")
                | ("getPath", "()Ljava/lang/String;")
        )
    {
        return true;
    }

    if matches!(
        class_name,
        "com/sun/tools/javac/util/Name"
            | "com/sun/tools/javac/util/SharedNameTable$NameImpl"
            | "com/sun/tools/javac/util/StringNameTable$NameImpl"
    ) && method_name == "equals"
        && method_descriptor == "(Ljava/lang/Object;)Z"
    {
        return true;
    }

    if class_name == "org/springframework/core/test/tools/CompileWithForkedClassLoaderExtension"
        && method_name == "isUsingForkedClassPathLoader"
        && method_descriptor == "(Lorg/junit/jupiter/api/extension/ExtensionContext;)Z"
    {
        return true;
    }

    // The SBR-02 fast-regex arm that named `String.replaceAll` /
    // `replaceFirst` / `matches` / `replace(CharSequence,CharSequence)` stood
    // here and is GONE with the rest of the `java/lang/String` policy. The four
    // natives themselves are NOT gone: they are the family this arm existed to
    // reach, they are 2x faster than HotSpot on the workload they were written
    // for (`probes/StringRegexCostProbe`), and they are now registered
    // `NativeKind::Intrinsic` — which is §1.4's own answer for a reviewed
    // same-result-just-faster native and needs no name list to win.
    //
    // This arm never made them win either. `CRATONVM_NATIVE_STRING_REGEX=0`
    // does not register them at all, and when it is set they are found by
    // `resolve_step1_native` on the triple alone. The gate that decides is
    // registration; see `NativeMethodRegistry::register`'s `java/lang/String`
    // real-JDK drop, which is where the reviewed set is now stated once.
    //
    // `CRATONVM_NATIVE_MATCHER_FIND`: real-JDK-layout `Matcher.find()`/
    // `find(int)`/`start`/`end`/`group` fast path (`native_matcher_find_realjdk`
    // et al. in native-builtins/src/lib.rs). Extends the SBR-02 fast-regex
    // idea above from the `String` convenience methods to the explicit
    // `Pattern.compile(...).matcher(...)` + `while (m.find()) { m.group(N); }`
    // idiom, which SBR-02 does nothing for (that idiom never calls
    // `String.replaceAll`/etc.) and still runs the interpreted engine.
    // `start`/`end`/`group` are included because they're on the same hot
    // loop and only read state `find`/`find(int)` already populate — leaving
    // them interpreted would still leave most of the per-iteration cost on
    // the table. Opt-in (default OFF; see `env_cache::native_matcher_find`)
    // pending the same parity validation SBR-02 went through before its flag
    // flipped default-ON.
    if class_name == "java/util/regex/Matcher"
        && crate::runtime::env_cache::native_matcher_find()
        && matches!(
            (method_name, method_descriptor),
            ("find", "()Z")
                | ("find", "(I)Z")
                | ("start", "()I")
                | ("start", "(I)I")
                | ("end", "()I")
                | ("end", "(I)I")
                | ("group", "()Ljava/lang/String;")
                | ("group", "(I)Ljava/lang/String;")
        )
    {
        return true;
    }
    // java.net.DatagramSocket / MulticastSocket — real-JDK delegate architecture.
    // Since JDK 14 these classes are thin wrappers that forward every operation
    // to an internal `delegate` (a `DatagramSocketImpl`-backed socket) created
    // lazily; the real bytecode for setOption/getOption/joinGroup/send/receive/…
    // calls `delegate()`, which throws `InternalError("Should not get here")`
    // when the delegate was never wired up. CratonVM models these sockets
    // natively (fd_table-backed, fields port/closed/timeout/fd[/ttl]) and never
    // populates the JDK `delegate`, so the concrete inherited bytecode (e.g.
    // `DatagramSocket.setOption`) always fails. Force our natives to win for the
    // operation surface Tomcat Tribes' `McastServiceImpl` drives (its `socket`
    // field is statically typed `MulticastSocket`, so the CP class is
    // MulticastSocket even for DatagramSocket-declared methods; both classes are
    // listed to be robust to either resolution). Constructors already dispatch
    // to natives via invokespecial and need no entry here.
    if matches!(
        class_name,
        "java/net/MulticastSocket" | "java/net/DatagramSocket"
    ) && matches!(
        method_name,
        "setOption"
            | "getOption"
            | "joinGroup"
            | "leaveGroup"
            | "setSoTimeout"
            | "getSoTimeout"
            | "setTimeToLive"
            | "getTimeToLive"
            | "setReuseAddress"
            | "getReuseAddress"
            | "setBroadcast"
            | "getBroadcast"
            | "send"
            | "receive"
            | "close"
            | "isClosed"
            | "getLocalPort"
    ) {
        return true;
    }
    // TYPE_USE annotation surface (JSpecify @Nullable/@NonNull): the real-JDK
    // getAnnotated{ReturnType,Type}/AnnotatedTypeBaseImpl bytecode can't decode
    // our null getTypeAnnotationBytes0 + unexposed ConstantPool. Single source
    // of truth — `check_override` (vm_exec.rs) consults the same predicate.
    if is_typeuse_annotation_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_reflection_access_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate HQL and Groovy route through ANTLR's prediction-context hot
    // loop during cold full-context parsing. These helpers are tiny
    // bytecode-equivalent methods; forcing the registered intrinsics removes
    // millions of interpreter frame transitions without changing parser
    // semantics.
    if is_antlr_prediction_context_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate/ByteBuddy proxy generation spends a large fraction of cold
    // setup in these tiny cached token hash/equals methods. Force the registered
    // bytecode-equivalent intrinsics to avoid thousands of interpreted
    // AbstractList iterator frames while ByteBuddy builds method graphs.
    if is_bytebuddy_method_token_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate's test extensions call this helper before every test method.
    // The real body delegates to JUnit's recursive composed-annotation scanner;
    // our native checks the same effective method/class locations directly.
    if is_hibernate_testing_util_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate Models stores annotation usages in a Map behind tiny default
    // interface methods. Force bytecode-equivalent natives to remove a hot
    // interpreted layer while FunctionTests repeatedly builds metadata.
    if is_hibernate_models_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // H2's MVStore transaction bookkeeping uses java.util.BitSet in the
    // Hibernate FunctionTests schema-drop path. These single-bit methods are
    // bytecode-equivalent intrinsics and avoid a hot interpreted cleanup loop.
    if is_bitset_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // H2's SQL parser cursor/token accessors are tiny methods called heavily
    // while Hibernate creates and drops schemas in FunctionTests. The native
    // versions are bytecode-equivalent and keep the parser moving under the
    // external harness cap.
    if is_h2_parser_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate's metadata boot path repeatedly builds property accessor names
    // through `new String(char[], int, int, Void)`, whose real-JDK body spends
    // most of its time in `StringLatin1.inflate`. The native is bytecode-
    // equivalent for the Latin-1 byte[] -> char[] copy and avoids millions of
    // interpreted inner-loop frames.
    if is_jdk_string_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Tiny JDK wrapper arithmetic helpers are already registered as exact
    // natives in `phases_early`; route real-JDK bytecode through them so hot
    // collection reductions such as Hibernate's JoinedList constructor do not
    // spin through one-frame interpreted helpers.
    if is_jdk_wrapper_math_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // CountDownLatch is registered as a synthetic monitor-backed native because
    // the real JDK body stores an AQS Sync object and parks through Unsafe /
    // LockSupport machinery CratonVM does not model completely. Force the full
    // public surface, including <init>, so the synthetic int[] holder is
    // installed before await/countDown read it.
    if is_count_down_latch_native_override(class_name, method_name, method_descriptor)
        || is_stamped_lock_native_override(class_name, method_name, method_descriptor)
    {
        return true;
    }
    if is_ffm_symbol_lookup_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_ffm_group_layout_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_ffm_memory_layout_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // FFM Arena lifecycle. Reaching it from here is what wires the exemption
    // into `try_stackless_invoke`'s step-6 interface guard (via
    // `should_force_registered_native_over_bytecode`), so that path agrees with
    // the explicit `force_ffm_arena_interface_native` term in
    // `invoke_on_class_shared`. See `is_ffm_arena_native_override`.
    if is_ffm_arena_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_file_channel_impl_open_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_file_system_provider_link_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_input_stream_transfer_to_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_zip_output_primitive_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Keep the cached virtual-call path aligned with vm_exec's
    // FileSystemProvider.newFileChannel override. The real base method is a
    // deliberate UnsupportedOperationException stub; the registered native
    // constructs CratonVM's fd-backed FileChannel for the default provider.
    if matches!(
        class_name,
        "java/nio/file/spi/FileSystemProvider"
            | "sun/nio/fs/WindowsFileSystemProvider"
            | "sun/nio/fs/UnixFileSystemProvider"
    )
        && method_name == "newFileChannel"
        && method_descriptor
            == "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;"
    {
        return true;
    }
    // Native-backed ZIP metadata uses the real JDK field layout but may carry
    // a null optional comment. Keep the bridge for the nullable setter so a
    // Spring Boot nested-entry copy does not enter ZipEntry's CEN validation
    // path with compact/native state.
    if matches!(
        class_name,
        "java/util/zip/ZipEntry" | "java/util/jar/JarEntry"
    ) && method_name == "setComment"
        && method_descriptor == "(Ljava/lang/String;)V"
    {
        return true;
    }
    if is_native_thread_set_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_java_nio_access_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_direct_buffer_pool_counter_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_stamped_lock_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_xerces_cmstateset_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_xerces_xml_parser_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_liquibase_checksum_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // JBoss Marshalling calls real-JDK `sun.reflect.ReflectionFactory`
    // bytecode to discover serialization hooks. On CratonVM the registered
    // natives encode ObjectStreamClass's private/inheritable hook rules and
    // must win over the bytecode body, or MethodHandle.invoke later tries to
    // dispatch `java/lang/Object.readObject(ObjectInputStream)`.
    if is_reflection_factory_serialization_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    // Surefire fork bootstrap/teardown: bypass ServiceLoader decoder discovery
    // and the acknowledgedExit semaphore path, both of which rely on JDK
    // internals CratonVM shadows with registered natives.
    if class_name == "org/apache/maven/surefire/booter/ForkedBooter"
        && matches!(method_name, "lookupDecoderFactory" | "acknowledgedExit")
    {
        return true;
    }
    // WF-XNIO: `OptionMap$Builder.addAll(OptionMap)` copies through
    // `OptionMap.iterator()`. Our XNIO map/builder state lives in native side
    // tables, and the synthetic array iterator can resolve as
    // `java/lang/Object.next()` through this path. Force the registered native
    // to copy entries directly; companion gate in vm_exec.rs.
    if class_name == "org/xnio/OptionMap$Builder"
        && method_name == "addAll"
        && method_descriptor == "(Lorg/xnio/OptionMap;)Lorg/xnio/OptionMap$Builder;"
    {
        return true;
    }
    matches!(
        (class_name, method_name, method_descriptor),
        ("java/lang/ClassLoader", "setDefaultAssertionStatus", "(Z)V")
            // ServiceLoader-based JDK facilities (including AttachProvider)
            // obtain their loader through Class.getClassLoader().  The real
            // body reads host-layout fields, while CratonVM's native validates
            // and returns the VM-owned loader; without this override a stale
            // String-shaped slot reaches ClassLoader.findResources.
            | ("java/lang/Class", "getClassLoader", "()Ljava/lang/ClassLoader;")
            // Startup javaagents receive a real-JDK
            // sun.instrument.InstrumentationImpl.  Its constructor calls
            // VM-private initialization that CratonVM does not expose; the
            // registered constructor is intentionally a no-op because the
            // observable Instrumentation operations are supplied by our
            // native bridge.  It must therefore beat the real bytecode just
            // like the other layout-backed native overrides in this table.
            | (
                "sun/instrument/InstrumentationImpl",
                "<init>",
                "(JLjava/lang/String;ZZ)V",
            )
            | (
                "java/lang/Thread",
                "getContextClassLoader",
                "()Ljava/lang/ClassLoader;",
            )
            | (
                "java/lang/Thread",
                "setContextClassLoader",
                "(Ljava/lang/ClassLoader;)V",
            )
            | (
                "com/sun/tools/attach/VirtualMachine",
                "attach",
                "(Ljava/lang/String;)Lcom/sun/tools/attach/VirtualMachine;",
            )
            | (
                "com/sun/tools/attach/VirtualMachine",
                "loadAgent",
                "(Ljava/lang/String;Ljava/lang/String;)V",
            )
            | (
                "com/sun/tools/attach/VirtualMachine",
                "loadAgent",
                "(Ljava/lang/String;)V",
            )
            | ("com/sun/tools/attach/VirtualMachine", "detach", "()V")
            | (
                "java/lang/ClassLoader",
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
            )
            | (
                "java/lang/ClassLoader",
                "loadClass",
                "(Ljava/lang/String;Z)Ljava/lang/Class;",
            )
            // `EndElementEvent.getNamespaces()` — the JDK Xerces StAX event impl
            // hard-codes an empty `ReadOnlyIterator` return (it computes
            // `fNamespaces.iterator()` then pops it). Our synthetic cursor reports
            // end-element namespaces (getNamespaceCount/Prefix/URI) and the
            // allocator fills `fNamespaces`, but this getter drops them, so
            // Spring's StaxEventXMLReader emits no `endPrefixMapping`
            // (StaxEventXMLReaderTests namespace methods). Force our native, which
            // returns the actual `fNamespaces` iterator — the behaviour of a
            // spec-correct provider (Woodstox is what HotSpot resolves for this
            // suite). Companion native: `native-builtins/src/xml_stax.rs`.
            | (
                "com/sun/xml/internal/stream/events/EndElementEvent",
                "getNamespaces",
                "()Ljava/util/Iterator;",
            )
            | ("java/net/URL", "getHost", "()Ljava/lang/String;")
            | (
                "java/net/URL",
                "setURLStreamHandlerFactory",
                "(Ljava/net/URLStreamHandlerFactory;)V"
            )
            // `Iterator.remove()V` is a default method that throws
            // `UnsupportedOperationException("remove")`. Several of our
            // synthetic iterator classes (`HashMap$KeyItr` built from
            // `HashSet.iterator()`) are pure synthetic stubs that don't
            // declare `java.util.Iterator` as an interface, so the
            // class-hierarchy-walk fallbacks in `invoke_on_class_shared_inner`
            // / `try_stackless_invoke` resolve through the CP-class default
            // method and execute that throwing body before the receiver-
            // class native lookup gets a chance. Forcing the registered
            // dispatcher in `native-collections` (which routes by receiver
            // class) here moves the receiver-class probe to the front of
            // every dispatch path — required for WildFly 39 / Keycloak 16's
            // `MXBeanSupport.findMXBeanInterface` `it.remove()` reduction
            // loop to succeed.
            | ("java/util/Iterator", "remove", "()V")
            // `ConstantCallSite.getTarget`. On JDK 25 the body is no
            // longer a plain `getfield target` — it first reads
            // `private boolean isFrozen` and throws
            // `IllegalStateException` when it's still false. Our
            // `LambdaMetafactory.metafactory` / `altMetafactory`
            // synthesise `ConstantCallSite` instances via
            // `alloc_concurrent_synthetic`, which bypasses the JDK
            // `<init>` body that flips `isFrozen=true`. Without the
            // force-native here, every callsite materialised by an
            // invokedynamic bootstrap throws ISE on first `getTarget`
            // — observed in `org.apache.logging.log4j`'s
            // `ServiceLoaderUtil.callServiceLoader` chain on
            // Elasticsearch and Spark log4j boot. The registered
            // native in `native-builtins/src/lang_invoke.rs` just
            // returns field 0 (the target MH) — the correct
            // behaviour for an effectively-frozen ConstantCallSite.
            | (
                "java/lang/invoke/ConstantCallSite",
                "getTarget",
                "()Ljava/lang/invoke/MethodHandle;",
            )
            | (
                "java/lang/invoke/ConstantCallSite",
                "dynamicInvoker",
                "()Ljava/lang/invoke/MethodHandle;",
            )
            // `JMXConnectorFactory.newJMXConnector(JMXServiceURL, Map)` —
            // the real-JDK bytecode chain `connect -> newJMXConnector ->
            // ServiceLoader.load(JMXConnectorProvider)` finds zero
            // providers because the RMI client provider is declared via
            // `module-info: provides ... with com.sun.jmx.remote.protocol.
            // rmi.ClientProvider`, not via a `META-INF/services/...`
            // descriptor, and our ServiceLoader (service_loader.rs) only
            // reads the classpath descriptor form. The factory then
            // throws `MalformedURLException("Unsupported protocol: rmi")`,
            // surfaced by Cassandra's nodetool as the misleading
            // "Failed to connect … - MalformedURLException: 'Unsupported
            // protocol: rmi'.". The registered native in jmx.rs
            // (`register_jmx_connector_factory`) instead raises a plain
            // `IOException("JMX over RMI is not implemented …")` so the
            // client's catch handler reports a connection-layer error.
            | (
                "javax/management/remote/JMXConnectorFactory",
                "newJMXConnector",
                "(Ljavax/management/remote/JMXServiceURL;Ljava/util/Map;)Ljavax/management/remote/JMXConnector;",
            )
            // `ManagementFactory.getGarbageCollectorMXBeans()` —
            // real-JDK bytecode delegates to `ManagementFactoryHelper`
            // which iterates platform GCs via natives we don't ship,
            // returning an empty list. H2 `Utils.collectGarbage()` loops
            // until `getCollectionTime()` ticks (`Utils.java:288-294`);
            // an empty bean list makes the loop infinite (>1h hang on
            // TestAll boot before any test runs). Force our synthetic
            // single-bean list (jmx.rs:966-980) backed by the real heap
            // GC counter so `collectGarbage()` exits after one cycle.
            | (
                "java/lang/management/ManagementFactory",
                "getGarbageCollectorMXBeans",
                "()Ljava/util/List;",
            )
            // `Hashtable.keys()` / `elements()` — the legacy pre-1.2
            // Enumeration accessors. We native-override put/get/size onto
            // our own side-store (`native_map_put` etc.), so the real-JDK
            // bytecode body (`return this.getEnumeration(KEYS)`) walks an
            // EMPTY internal `Hashtable.table[]` and returns an empty
            // Enumeration. Sound-but-different contract: real JDK is
            // correct for its own table, but our backing store is in a
            // different place. BC's `AbstractX500NameStyle.copyHashTable`
            // depends on `keys()` to populate the per-instance
            // `defaultLookUp`; without this override, every
            // `attrNameToOID("cn"/"o"/"CN"/...)` returns null and the
            // X.500 RDN parser throws "Unknown object id".
            | (
                "java/util/Hashtable",
                "keys",
                "()Ljava/util/Enumeration;",
            )
            | (
                "java/util/Hashtable",
                "elements",
                "()Ljava/util/Enumeration;",
            )
            // TC0622: `Hashtable.clone()` (inherited by `Properties`). Our
            // native `put` stores synthetic bucket nodes in slot-0 `table[]`,
            // not genuine `Hashtable$Entry`. The real-JDK clone body does
            // `t.table[i] = (Hashtable$Entry) table[i].clone()` and the
            // `checkcast` throws ClassCastException on our synthetic node.
            // (`InitialContext.<init>` clones its environment Hashtable, so
            // `new InitialDirContext(env)` blew up before any LDAP connect.)
            // Force the native (deprecated_util::native_hashtable_clone) which
            // rebuilds a fresh natively-backed map without materialising an
            // Entry. Companion match in vm_exec.rs.
            | (
                "java/util/Hashtable",
                "clone",
                "()Ljava/lang/Object;",
            )
            // spring-bug-08: `ObjectInputStream.resolveProxyClass(String[])`
            // has a real JDK body whose default routes
            // `Proxy.getProxyClass` → `ProxyBuilder.getDynamicModule` →
            // `Module.defineModule0` (a native the synthetic proxy model can't
            // satisfy → `UnsatisfiedLinkError`/`ClassNotFoundException: null`).
            // Force CratonVM's registered native (serialization.rs), which
            // returns a generated `$ProxyN` class directly, so a serialized JDK
            // dynamic proxy round-trips on CratonVM's own proxy machinery. Only
            // a plain `java/io/ObjectInputStream` is forced — a subclass that
            // overrides `resolveProxyClass` dispatches under its own class name
            // and keeps its override.
            | (
                "java/io/ObjectInputStream",
                "resolveProxyClass",
                "([Ljava/lang/String;)Ljava/lang/Class;",
            )
            // proxy-real-classfile increment 6: `InvocationHandler.invokeDefault`
            // (static, JDK 16+). The real JDK body drives `Proxy.invokeDefault`,
            // which reflects the generated proxy class's `proxyClassLookup`
            // accessor + a full-power `MethodHandles.Lookup` to bind an
            // invokespecial MethodHandle to the interface default body. CratonVM's
            // generated `$ProxyN` emits no `proxyClassLookup` (and the proxy model
            // has no real per-class Lookup), so the real bytecode throws
            // `InternalError: NoSuchMethodException: proxyClassLookup`. Force the
            // registered native (native-builtins
            // `native_invocation_handler_invoke_default`), which runs the default
            // body directly via `invoke_special`.
            | (
                "java/lang/reflect/InvocationHandler",
                "invokeDefault",
                "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
            )
            // proxy-real-classfile increment 7: deprecated `Proxy.getProxyClass`.
            // The real JDK body routes the dynamic-module machinery
            // (`ProxyBuilder.getDynamicModule` → `Module.defineModule0`) the
            // synthetic proxy model can't satisfy → `InternalError: Proxy is not
            // supported until module system is fully initialized`. Force the
            // registered native (native-builtins `native_proxy_get_proxy_class`),
            // which returns the generated `$ProxyN` class directly.
            | (
                "java/lang/reflect/Proxy",
                "getProxyClass",
                "(Ljava/lang/ClassLoader;[Ljava/lang/Class;)Ljava/lang/Class;",
            )
            // TC0622 classpath:-protocol: `jdk.internal.misc.VM.isBooted()` on
            // JDK 25 is real bytecode `return initLevel >= SYSTEM_BOOTED(4)`,
            // reading the *static field* `jdk.internal.misc.VM.initLevel`.
            // CratonVM boots natively and never runs the real
            // `System.initPhase2/3` that would call `VM.initLevel(int)` to set
            // that field, so it stays 0 and the real `isBooted()` returns false
            // forever. `java.net.URL.getURLStreamHandler` gates factory lookup
            // on `isOverrideable(protocol) && VM.isBooted()`, so a false result
            // makes the un-intercepted real `getURLStreamHandler` skip the
            // app-installed `URLStreamHandlerFactory` entirely and throw
            // `MalformedURLException: unknown protocol: classpath` even though
            // Tomcat's `TomcatURLStreamHandlerFactory` is correctly registered
            // (and published into `URL.factory` by
            // `native_url_set_stream_handler_factory_guard`, HIB-CV-15). The
            // registered native (`register_essential_natives`, lib.rs) returns
            // 1; force it so the boot-state native — like the `VM.initLevel()`
            // floor-of-2 native alongside it — actually shadows the real
            // bytecode. By the time any app/JDK-library code calls `isBooted()`
            // the VM is genuinely up, matching HotSpot's post-boot `true`. Also
            // unblocks the JASPIC `ResourcesMgr` path (commit 873355f1 added the
            // native but missed this force-list entry, leaving it inert).
            | ("jdk/internal/misc/VM", "isBooted", "()Z")
    ) || (class_name == "java/net/URL"
        && matches!(method_name, "getAuthority" | "getHostAddress"))
        || (matches!(
            class_name,
            "java/net/InetAddress" | "java/net/Inet4Address" | "java/net/Inet6Address"
        ) && matches!(
            method_name,
            "getHostName" | "getCanonicalHostName" | "getHostAddress"
        ))
        // URLClassLoader.findClass / findResource / findResources +
        // URLClassPath.addURL — see the companion `check_override` entry in
        // `vm_exec.rs::invoke_on_class_shared_inner`. The real bytecode routes
        // through the shimmed `URLClassPath` (null `unopenedUrls`/`path`), so
        // `addURL` NPEs and `findClass`/`findResource(s)` find nothing. Force
        // the natives (`ucp_add_url` / `ucl_find_class` / `ucl_find_resource(s)`)
        // on the bytecode-interpreter + cached/promoted dispatch paths. `addURL`
        // is keyed on `URLClassPath` (its `ucp.addURL(url)` call site) — the
        // `URLClassLoader.addURL` wrapper is invoked via a subclass `this`,
        // escaping this static-class gate. Hibernate `NoDepthTests` JPA +
        // ShrinkWrap.
        //
        // `findClass` matters when a `URLClassLoader` subclass overrides BOTH
        // `loadClass` overloads and calls `findClass` directly (so CratonVM's
        // `cl_load_class` native — which would otherwise resolve from the global
        // classpath — never runs): Jasper's `JasperLoader` does exactly this to
        // load the runtime-compiled `org.apache.jsp.*_jsp` servlet from its
        // scratch-dir URL. Without forcing the native, the real
        // `URLClassLoader.findClass` reaches the shimmed `ucp.getResource` →
        // null → `ClassNotFoundException`, 500-ing every compiled JSP/tag
        // (TestPageContext, TestScopedAttributeELResolver, …). `ucl_find_class`
        // delegates to the base classpath (where `<init>` already registered the
        // loader's URLs), matching HotSpot.
        || (class_name == "java/net/URLClassLoader"
            && (matches!(method_name, "findClass" | "findResource" | "findResources" | "getURLs" | "addURL" | "close")
                || (method_name == "<init>"
                    && matches!(
                        method_descriptor,
                        "([Ljava/net/URL;)V"
                            | "([Ljava/net/URL;Ljava/lang/ClassLoader;)V"
                            | "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;)V"
                            | "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V"
                            | "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V"
                            | "([Ljava/net/URL;Ljava/security/AccessControlContext;)V"
                            | "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/security/AccessControlContext;)V"
                    ))))
        || (matches!(
            class_name,
            "jdk/internal/loader/URLClassPath" | "sun/misc/URLClassPath"
        ) && method_name == "addURL")
}

/// True when `class_name` has been redefined in place by a JVMTI agent
/// (e.g. Mockito's inline mock maker), so its woven bytecode is authoritative
/// and the VM's per-class native/intrinsic shadows must be suppressed — the
/// woven advice has to run for the mock to intercept (matching HotSpot, which
/// always executes the retransformed bytecode).
///
/// Fast-pathed on the global [`any_class_redefined`] flag: until some agent
/// redefines a class (the overwhelming common case) this is a single relaxed
/// atomic load and never touches the class-manager lock. Once armed, it costs
/// one `class_manager` read + a generation lookup, but only at the handful of
/// dispatch sites that were about to serve a native/intrinsic shadow.
#[inline]
pub(crate) fn native_shadow_suppressed_by_redefine(shared: &SharedVm, class_name: &str) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    native_shadow_suppressed_in(&cm, class_name)
}

/// Reflection-metadata natives on the `java.lang.reflect.*` member types that
/// must stay authoritative even after their declaring class is redefined.
///
/// CratonVM serves these (annotations, parameter annotations, annotation
/// defaults, annotated types) from VM-side structures, NOT from raw class-file
/// bytes a real `sun.reflect.annotation.AnnotationParser` + `ConstantPool`
/// could decode. So the suppress-native-shadow-on-redefine guard — which
/// otherwise correctly cedes a redefined class's methods to their woven
/// bytecode so a Mockito inline mock's advice runs — must NOT fire for these.
///
/// The trigger: `Mockito.mock(java.lang.reflect.Method.class)` inline-redefines
/// `java.lang.reflect.Method`. That bumps its `redefine_generation`, so EVERY
/// subsequent `Method.getDeclaredAnnotations()` / `isAnnotationPresent(...)`
/// call — on ANY method object, not just the mocked class — was routed to the
/// real `Executable.declaredAnnotations()` bytecode, which under CratonVM reads
/// empty annotation bytes and returns no annotations. JUnit's `@Test` scan then
/// finds zero test methods and Spring AOP's `MethodMatchersTests` (whose
/// `static final Method TEST_METHOD = mock(Method.class)` runs at class-init)
/// discovers 0 tests where HotSpot runs 14. A mock only needs its per-INSTANCE
/// dispatch woven; these class-level metadata accessors are not instance
/// behaviour and the mock never stubs them, so keeping the native is correct
/// (and matches HotSpot, where the redefine leaves real annotation reflection
/// intact). Business-method inline mocks (e.g. `InetAddress.getHostName`) are
/// unaffected — they are not in this list.
pub(super) fn redefine_immune_reflection_native(class_name: &str, method_name: &str) -> bool {
    matches!(
        class_name,
        "java/lang/reflect/Method"
            | "java/lang/reflect/Constructor"
            | "java/lang/reflect/Field"
            | "java/lang/reflect/Executable"
            | "java/lang/reflect/AccessibleObject"
    ) && matches!(
        method_name,
        "getDeclaredAnnotations"
            | "getAnnotations"
            | "getAnnotation"
            | "getDeclaredAnnotation"
            | "isAnnotationPresent"
            | "getAnnotationsByType"
            | "getDeclaredAnnotationsByType"
            | "getParameterAnnotations"
            | "getDefaultValue"
            | "getAnnotatedReturnType"
            | "getAnnotatedParameterTypes"
            | "getAnnotatedExceptionTypes"
            | "getAnnotatedReceiverType"
    )
}

/// Methods whose real JDK bodies access compact `byte[]`/`coder`/`count`
/// fields while CratonVM StringBuilder objects intentionally use a synthetic
/// `char[]`/`count` layout. A registered native must win for every one of these
/// operations, including direct methods on StringBuilder rather than only their
/// AbstractStringBuilder implementation.
pub(crate) fn is_string_builder_layout_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    // `java/lang/StringBuffer` is NOT here, and its absence is load-bearing.
    // Its 62 natives were retired from the real-JDK registrar (see the block
    // at `register_string_builder_natives`'s call site in
    // `native-builtins/src/lib.rs`) so that its own `synchronized` bodies run
    // and supply the monitor and the `toStringCache` invalidation a shared
    // native cannot. Leaving the class here would let this gate resolve
    // `StringBuffer.append` by walking to the INHERITED
    // `AbstractStringBuilder.append` native and running it directly, skipping
    // the `StringBuffer` body entirely — a retirement that is silently undone
    // by the gate that outlived it.
    if !matches!(
        class_name,
        "java/lang/StringBuilder" | "java/lang/AbstractStringBuilder"
    ) {
        return false;
    }
    if matches!(
        method_name,
        // Keep this deliberately narrow: these direct JDK bodies read the
        // incompatible compact-string layout on synthetic builders. Other
        // operations keep their established dispatch to avoid turning the
        // high-volume AOT code-generation path into an all-native slow path.
        //
        // `setCharAt` joined this list once the invoke-cache redefine guards
        // became precise enough to actually evict a stale native shadow for
        // a genuinely real (non-mock) StringBuilder after an UNRELATED
        // Mockito.mock(StringBuilder.class) redefined the class elsewhere in
        // the process: real `AbstractStringBuilder.setCharAt`'s bytecode
        // writes through `String.checkIndex` against the compact
        // byte[]/coder layout, which CratonVM's synthetic builder doesn't
        // have, so it AIOOBE'd instead of writing the synthetic char[].
        //
        // 2026-07-31: the list below `setCharAt` was still incomplete, and
        // `SbMethodMatrixProbe` (Spring-free: run each builder operation on a
        // REAL builder before and after an unrelated
        // `Mockito.mock(StringBuilder.class)`) named the survivors exactly.
        // Six operations silently changed behaviour after the redefinition:
        //
        //   setLength(4)   'abcdefghij' -> len=4 but toString() == "a"
        //   setLength(0)   left one stale char behind
        //   deleteCharAt   'abcdefghij' -> len=9 but toString() == "a"
        //   replace        ArrayStoreException (src=Byte, dest=Char)
        //   ensureCapacity buffer overwritten with spaces
        //   trimToSize     same
        //   repeat         appended nothing
        //
        // Each has a registered native in `register_string_builder_natives`
        // (`setLength(I)V`, `deleteCharAt(I)…`, `replace(IILjava/lang/String;)…`,
        // `ensureCapacity(I)V`, `trimToSize()V`, `repeat(II)…`), so before the
        // redefinition they all dispatched native and were correct; the
        // redefinition evicted the shadow and handed them back to real JDK
        // bodies that index a compact `byte[] value` / `byte coder` /
        // `int count` layout CratonVM's two-field `char[]`/`int` builder does
        // not have. Adding them here is the same trade `setCharAt` already
        // makes: these mutate builder state, and nothing stubs them on a mock.
        // `capacity`, `getCoder`, `getValue`, `reverse` and the `codePoint*`
        // readers join for the same reason — they read `value`/`coder`
        // directly and cannot be expressed against the synthetic layout.
        //
        // `length()` and `substring(int)` stay OUT of this list on purpose;
        // see the long note below. They are the two operations
        // `MockitoBeanByTypeLookupIntegrationTests` genuinely stubs and
        // verifies on a mocked StringBuilder, so their native shadow must
        // stay evictable for Mockito's woven advice to run.
        "<init>"
            | "append"
            | "capacity"
            | "charAt"
            | "codePointAt"
            | "codePointBefore"
            | "codePointCount"
            | "delete"
            | "deleteCharAt"
            | "ensureCapacity"
            | "getChars"
            | "getCoder"
            | "getValue"
            | "insert"
            | "repeat"
            | "replace"
            | "reverse"
            | "setCharAt"
            | "setLength"
            | "toString"
            | "trimToSize"
    ) {
        return true;
    }
    // `length()` FIX (2026-07-23, follow-up to the KNOWN GAP left by the
    // previous session): stop blanket-immunizing "length" for ANY of the
    // three class names -- matching `substring(int)`'s existing treatment
    // exactly (that method is not, and never has been, in this list).
    //
    // Ground truth, captured by dumping Mockito's OWN redefined bytecode on
    // real HotSpot (`-Dnet.bytebuddy.dump=...`, JDK 25, Mockito 5.23.0):
    // `Mockito.mock(StringBuilder.class)` redefines BOTH `StringBuilder`
    // (whose `length()` is a compiler-generated public bridge --
    // `AbstractStringBuilder` is package-private -- confirmed via `javap -p
    // -c java.lang.StringBuilder`: `aload_0; invokespecial
    // AbstractStringBuilder.length:()I; ireturn`, UNCHANGED by redefinition)
    // AND `AbstractStringBuilder` itself, weaving the actual
    // `MockMethodDispatcher.get/isMocked/isOverridden/handle` advice
    // directly into `AbstractStringBuilder.length()`'s own body, ahead of
    // its original `getfield count:I` tail. So blanket-forcing native for
    // `AbstractStringBuilder.length()` (an earlier version of this fix kept
    // that arm immune, reasoning the bridge alone was the redefined method,
    // by analogy with `substring`) permanently pre-empted the advice for
    // BOTH a mock AND a real receiver of the class -- the STRINGBUILDER
    // bridge's `invokespecial` reached `AbstractStringBuilder.length()`,
    // which our own force-native gate intercepted before Mockito's advice
    // ever got to run. Removing immunity here (verified against
    // `InvocationCountProbe`, which reflects
    // `Mockito.mockingDetails(mock).getInvocations()`) now byte-for-byte
    // matches real HotSpot's `length()`/`substring(0)`/`verify()` sequence.
    //
    // KNOWN REMAINING GAP: a REAL (non-mock) receiver's `.length()`, called
    // AFTER some OTHER StringBuilder has been Mockito-redefined ANYWHERE in
    // the process, now falls through the woven advice's "not mocked" branch
    // into `AbstractStringBuilder.length()`'s original `getfield count:I` --
    // which reads the wrong field index against CratonVM's 2-field
    // (`char[]`, `int`) synthetic layout (real JDK's compiled class expects
    // `value`/`coder`/`count` at indices 0/1/2) and silently returns `0`
    // instead of the real length (confirmed via a dedicated probe:
    // `RealAfterMockLengthProbe`, `/data/tmp/mockitobean-substring-20260723/`).
    // This is the EXACT SAME latent risk `substring(int)` has carried,
    // unaddressed, since bug 3 of this class's fix history -- not a
    // regression this change introduces, just the same known tradeoff now
    // also applying to `length()`. Fixing it for real needs an authoritative
    // per-instance "is this receiver actually mocked" signal reachable from
    // Rust WITHOUT re-entering bytecode dispatch for the same (class,
    // method) pair (a naive `MockUtil.isMock` + re-invoke attempt during
    // this session's investigation infinite-looped, since re-invoking
    // "this method's bytecode" from inside the very native registered for
    // it re-triggers the identical force-native decision) -- left open, not
    // hit by any currently-passing suite class.
    if method_name == "length" && method_descriptor == "()I" {
        return false;
    }
    // `substring(int, int)` -- deliberately excludes `substring(int)`.
    // `substring(int)` must stay evictable: `MockitoBeanByTypeLookup*
    // IntegrationTests` explicitly stubs/verifies `.substring(anyInt())`
    // on a Mockito-mocked StringBuilder, which only works if the redefine
    // guards can drop this method's native shadow so the woven advice
    // actually runs (see the `Native{}` cache-hit redefine guard). But
    // `substring(int, int)`'s native shadow needs the SAME layout-safety
    // forcing as `setCharAt` above for a REAL (non-mock) receiver: Mockito
    // itself calls `new StringBuilder(...).substring(start, end)` inside
    // `StringUtil.join` (`Reporter.unfinishedVerificationException`'s
    // message formatting) on its own internal, never-mocked StringBuilder,
    // and once ANY StringBuilder in the process gets Mockito-redefined,
    // real `AbstractStringBuilder.substring(int,int)` bytecode AIOOBE'd
    // reading the incompatible compact layout -- masking the ACTUAL
    // "unfinished verification" failure behind a crash in the exception
    // message it was trying to construct. No test in this suite stubs or
    // verifies the two-arg overload, so forcing it native is safe.
    method_name == "substring" && method_descriptor == "(II)Ljava/lang/String;"
}

/// The layout-incompatible natives, for the invoke-cache dispatch sites.
///
/// These sites must not use `redefine_immune_forced_native`. Broadening them to
/// the full set — which additionally covers reflection metadata, JFR, BC crypto,
/// StampedLock and FileHandler — was measured on 2026-07-31 and **regressed**
/// ByteBuddy type creation: Spring AOT chunk 3 started failing roughly one run
/// in eight with `NoSuchMethodError: java.lang.Integer.isArray()Z` /
/// `Integer.represents(Type)Z` out of
/// `TypeDescription$Generic$Visitor$Substitutor`, a signature that appears in no
/// pre-change log across three full sweeps. 9/9 clean on the unmodified binary
/// under the same harness, 8/9 with the broadening. Those extra arms exist for
/// the *slow* path and are not safe to assert here.
///
/// What every member of this set has in common is narrower and checkable: the
/// receiver's real JDK body indexes a field layout CratonVM's object does not
/// have, so running it can only produce nonsense — whatever else is true about
/// the class.
pub(super) fn redefine_immune_layout_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    // The ZIP arm is DELIBERATELY absent here, and this is the one arm whose
    // two aggregators must NOT match.
    //
    // Every other member of this set is decided by the CLASS: a synthetic
    // collection, a VM-minted carrier, a `ThreadLocal`, a `StringBuilder` — for
    // those, every instance of the class is in the same position and a
    // receiver-blind cache entry is a correct one. `ZipFile`/`JarFile` are not:
    // a REAL archive keeps its handle in `jar_table()` and must stay on the
    // native, while a Mockito INLINE mock of the same class has no handle at
    // all and must reach the woven advice. Same class, same method, opposite
    // answers — which is exactly what a per-call-site cache cannot express, and
    // caching either answer is wrong for the other receiver.
    //
    // So the cache paths refuse to decide: with the arm gone, a redefined
    // `ZipFile`/`JarFile` fails their immunity check, the cached native is not
    // installed (or is evicted), and dispatch falls through to the slow path —
    // where `redefine_immune_forced_native_for_receiver` HAS the receiver and
    // answers per instance. A real archive still gets its native there; only
    // the fast path is given up, and only in a process that mocked one of these
    // two classes.
    //
    // `leaves_the_zip_arm_to_the_receiver_aware_slow_path` pins this, and says
    // so, because the standing rule for this file is the opposite one
    // (`thread_local_immunity_reaches_the_invoke_cache_sites_too`).
    redefine_immune_string_builder_native(class_name, method_name, method_descriptor)
        || redefine_immune_path_native(class_name, method_name, method_descriptor)
        || redefine_immune_synthetic_collection_native(class_name)
        || redefine_immune_vm_minted_carrier_native(class_name)
        || redefine_immune_thread_local_native(class_name)
}

pub(super) fn redefine_immune_string_builder_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    is_string_builder_layout_native_override(class_name, method_name, method_descriptor)
}

pub(super) fn redefine_immune_path_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name == "java/nio/file/Path"
        && method_name == "toString"
        && method_descriptor == "()Ljava/lang/String;"
}

pub(super) fn redefine_immune_jfr_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    is_jfr_metadata_native_override(class_name, method_name, method_descriptor)
}

pub(super) fn is_jfr_metadata_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    matches!(
        (class_name, method_name, method_descriptor),
        (
            "jdk/jfr/internal/Type",
            "getKnownType",
            "(Ljava/lang/Class;)Ljdk/jfr/internal/Type;"
        ) | (
            "jdk/jfr/internal/util/Utils",
            "getValidType",
            "(Ljava/lang/Class;Ljava/lang/String;)Ljdk/jfr/internal/Type;"
        ) | ("jdk/jfr/internal/JDKEvents", "initialize", "()V")
            | ("jdk/jfr/internal/instrument/JDKEvents", "initialize", "()V")
            | ("jdk/jfr/consumer/RecordingStream", "startAsync", "()V")
            | ("jdk/jfr/Event", "begin", "()V")
            | ("jdk/jfr/Event", "end", "()V")
            | ("jdk/jfr/Event", "commit", "()V")
            | ("jdk/jfr/Event", "isEnabled", "()Z")
            | ("jdk/jfr/Event", "shouldCommit", "()Z")
            | (
                "jdk/jfr/AnnotationElement",
                "checkType",
                "(Ljava/lang/Class;)V"
            )
            | ("jdk/jfr/Recording", "start", "()V")
            | ("jdk/jfr/Recording", "stop", "()Z")
            | ("jdk/jfr/Recording", "dump", "(Ljava/nio/file/Path;)V")
    )
}

/// Spring Boot's Mongo reactive lifecycle bean waits indefinitely on a Netty
/// promise that can remain incomplete after its event-loop workers are gone.
/// The native replacement requests shutdown and returns without that wait.
pub(crate) fn is_springboot_mongo_reactive_customizer_destroy_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name
        == "org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration$NettyDriverMongoClientSettingsBuilderCustomizer"
        && method_name == "destroy"
        && method_descriptor == "()V"
}

pub(crate) fn is_springboot_mongo_reactive_customizer_customize_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name
        == "org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration$NettyDriverMongoClientSettingsBuilderCustomizer"
        && method_name == "customize"
        && method_descriptor == "(Lcom/mongodb/MongoClientSettings$Builder;)V"
}

/// JDK 25's JNDI DNS client can use either `DatagramChannel` factory. Its real
/// `DatagramChannelImpl` path does not share CratonVM's fd-table state, so the
/// factories and the synthetic channel's local-address accessor must select
/// the native UDP bridge.
pub(crate) fn is_datagram_channel_open_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    if matches!(
        class_name,
        "java/nio/channels/DatagramChannel" | "java/nio/channels/NetworkChannel"
    ) && method_name == "getLocalAddress"
        && method_descriptor == "()Ljava/net/SocketAddress;"
    {
        return true;
    }
    if class_name == "java/nio/channels/DatagramChannel"
        && method_name == "open"
        && method_descriptor == "()Ljava/nio/channels/DatagramChannel;"
    {
        return true;
    }
    method_descriptor == "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;"
        && ((class_name == "java/nio/channels/DatagramChannel" && method_name == "open")
            || (class_name == "sun/nio/ch/SelectorProviderImpl"
                && method_name == "openDatagramChannel"))
}

/// `ZipFile`/`JarFile` archives live in a Rust handle table, not in the JDK's
/// `res`/`zsrc` field graph.
///
/// CratonVM serves the operations listed below from registered natives whose
/// state is a `jar_table()` entry keyed by a handle stashed on the object
/// (`native-io/src/zip_real_jar.rs`). The real `java.util.zip.ZipFile` body
/// reads `this.res.zsrc` -- a `CleanableResource` those natives never
/// populate -- so it can only ever NPE, for every archive the process has
/// opened. Exactly the shape of the synthetic-collection and `ThreadLocal`
/// arms above, and exactly the same trigger: Mockito's inline mock maker
/// instruments its target's whole superclass chain, so `spy()` of ANY
/// `JarFile` subclass retransforms `java.util.jar.JarFile` and
/// `java.util.zip.ZipFile` themselves, the suppress-native-shadow-on-redefine
/// rule fires, and the next `getInputStream` on ANY jar in the process runs
/// the real body against an instance that has no `res`.
///
/// Found by Spring Boot's `JarUrlConnectionTests`: 46 of its 47 tests pass,
/// and the 47th fails only when it runs after
/// `getInputStreamWhenNoCachedClosesJarFileOnClose`, the one test in the class
/// that calls `spy(jarFile)`:
///
/// ```text
/// java.lang.NullPointerException: Cannot read field "zsrc" because "<local3>.res" is null
///     at java.util.zip.ZipFile.getInputStream(ZipFile.java:327)
///     at java.util.jar.JarFile.getInputStream(JarFile.java:834)
/// ```
///
/// Method-wise rather than class-wide, and deliberately keyed to the SAME
/// method sets the two force-native gates already use (the `java/util/zip/
/// ZipFile` arm in this file's warm-cache policy and the `java/util/jar/
/// JarFile` arm in the cold-path twin). That is the principled boundary: a
/// method is forced to its native because the instance lacks the JDK field
/// graph it needs, and a redefinition cannot conjure that field graph -- so
/// every force-native method here is immune, and nothing else is. A `JarFile`
/// method NOT on that list is ordinary bytecode and stays evictable, so an
/// agent can still weave it.
/// `System.identityHashCode(obj)` computed from `shared` alone.
///
/// Byte-identical to `NativeContextImpl::identity_hash_code` (and therefore to
/// the `java/lang/System.identityHashCode` native, which is that method's only
/// caller), because the side table this key indexes is written from the native
/// side and read from here. A different derivation would look up under a key
/// nothing ever wrote.
pub(crate) fn vm_identity_hash(shared: &SharedVm, obj: ObjectRef) -> i32 {
    let heap = &shared.mem.heap;
    let heap_answer = heap.identity_hash_code(obj);
    shared
        .threads
        .monitors
        .java_identity_hash(obj, heap_answer, || heap.next_identity_hash())
}

/// Does the `ZipFile`/`JarFile` redefine immunity have to stand DOWN for this
/// receiver?
///
/// [`redefine_immune_zip_file_native`] is class-and-method scoped and says so:
/// a method is forced to its native "because the instance lacks the JDK field
/// graph it needs, and a redefinition cannot conjure that field graph". That
/// is the right rule for a real archive and the wrong one for a receiver that
/// is not an archive at all.
///
/// Mockito's inline mock maker mocks `java.util.jar.JarFile` by retransforming
/// `JarFile` and `ZipFile` themselves and instantiating the target through
/// objenesis -- no constructor runs, so `open_and_register` never gave the
/// object a handle. The immunity then keeps a native that has nothing to
/// answer from: `mock(JarFile.class).getName()` returns `null` and `.close()`
/// does nothing, and neither call is RECORDED, so
/// `then(jarFile).should().close()` surfaces as an unfinished verification on
/// the NEXT `mock()` in the class. Spring Boot's `UrlJarFilesTests` fails two
/// tests that way, each naming an EARLIER test whose `close()` never landed.
///
/// So the immunity is asked one more question -- is this receiver an archive
/// we actually opened? -- and stands down when the answer is no, letting
/// dispatch fall through to the woven bytecode where the mock advice lives.
///
/// Two deliberate narrowings:
///
///  * `<init>` is NEVER waived. A real archive's handle is registered BY the
///    constructor, so at its entry the receiver necessarily has none yet;
///    waiving there would cede every real `new JarFile(...)` in a redefined
///    process to a JDK body that cannot open it.
///  * The receiver must be in hand. At a gate that does not carry one the
///    immunity stands, which is the pre-existing behaviour.
///
/// The set behind `identity_is_known_archive` is append-only, so a CLOSED
/// archive still counts as real -- `close` is on the immunity list and a
/// double-close must not reach `ZipFile.close`'s real body, which reads the
/// `res` field CratonVM never populates.
pub(crate) fn zip_immunity_waived_for_receiver(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    if method_name == "<init>" {
        return false;
    }
    if !redefine_immune_zip_file_native(class_name, method_name) {
        return false;
    }
    let Some(obj) = receiver else {
        if dbg_zip_immune().is_some() {
            eprintln!("[zipimmune] {class_name}.{method_name} waiver: NO RECEIVER");
        }
        return false;
    };
    let known =
        cratonvm_native_io::zip_real_jar::identity_is_known_archive(vm_identity_hash(shared, obj));
    if dbg_zip_immune().is_some() {
        eprintln!(
            "[zipimmune] {class_name}.{method_name} waiver: known_archive={known} -> waived={}",
            !known
        );
    }
    !known
}

/// [`redefine_immune_forced_native`] with the receiver-aware ZIP waiver
/// applied. Every gate that HAS the receiver calls this instead.
pub(crate) fn redefine_immune_forced_native_for_receiver(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    redefine_immune_forced_native(class_name, method_name, method_descriptor)
        && !zip_immunity_waived_for_receiver(shared, class_name, method_name, receiver)
}

/// `CRATONVM_DBG_ZIPIMMUNE` — the ZIP redefine-immunity lever. Two values:
///
///  * `1` — trace: one line per distinct (class, method) the immunity is
///    consulted for, plus one per waiver decision. This is what named the last
///    receiver-blind gate: a `java/util/zip/ZipFile` consultation printed with
///    no waiver line beside it is a caller that is not asking about the
///    receiver.
///  * `off` — force the immunity to `false` everywhere. Behaviour-changing, and
///    the point: it separates "this gate decides" from "some other gate
///    decides" in one run, which three rounds of converting gates one at a time
///    could not.
///
/// Both are inert unless set, and the read is one `OnceLock`.
fn dbg_zip_immune() -> Option<&'static str> {
    use std::sync::OnceLock;
    static C: OnceLock<Option<String>> = OnceLock::new();
    C.get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_ZIPIMMUNE").ok())
        .as_deref()
}

pub(super) fn redefine_immune_zip_file_native(class_name: &str, method_name: &str) -> bool {
    let hit = redefine_immune_zip_file_native_inner(class_name, method_name);
    if let Some(mode) = dbg_zip_immune() {
        if hit {
            use parking_lot::Mutex;
            use std::sync::OnceLock;
            static SEEN: OnceLock<Mutex<std::collections::BTreeSet<(String, String)>>> =
                OnceLock::new();
            let seen = SEEN.get_or_init(|| Mutex::new(Default::default()));
            if seen
                .lock()
                .insert((class_name.to_string(), method_name.to_string()))
            {
                eprintln!("[zipimmune] {class_name}.{method_name} immune=true mode={mode}");
            }
        }
        if mode == "off" {
            return false;
        }
    }
    hit
}

fn redefine_immune_zip_file_native_inner(class_name: &str, method_name: &str) -> bool {
    match class_name {
        "java/util/zip/ZipFile" => matches!(
            method_name,
            "<init>"
                | "getEntry"
                | "getInputStream"
                | "entries"
                | "stream"
                | "getComment"
                | "close"
                | "getName"
                | "isMultiRelease"
                | "size"
        ),
        "java/util/jar/JarFile" => matches!(
            method_name,
            "<init>"
                | "getManifest"
                | "getManifestFromReference"
                | "stream"
                | "entries"
                | "getEntry"
                | "getJarEntry"
                | "getInputStream"
                | "size"
                | "close"
                | "getName"
        ),
        _ => false,
    }
}

/// The full immunity set, for the slow dispatch path.
///
/// The invoke-cache sites deliberately use the narrower
/// `redefine_immune_layout_native` instead — see the note there for the
/// measurement that says why. What both must share is the layout arm: when the
/// 2026-07-31 collections entry was added here only, the collection probe went
/// from 32 broken operations to 18 rather than to 0, because the cache sites
/// re-assembled their own chain and never saw it.
/// `layout_immunity_is_not_open_coded` keeps the two in step.
pub(crate) fn redefine_immune_forced_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    redefine_immune_reflection_native(class_name, method_name)
        || redefine_immune_string_builder_native(class_name, method_name, method_descriptor)
        || redefine_immune_path_native(class_name, method_name, method_descriptor)
        || redefine_immune_jfr_native(class_name, method_name, method_descriptor)
        || is_bc_crypto_math_native_override(class_name, method_name, method_descriptor)
        || is_stamped_lock_native_override(class_name, method_name, method_descriptor)
        // java.util.logging.FileHandler's registered natives store their
        // filename/closed bookkeeping in an identity-hash side table
        // (jul_file_handler_state_table, native-builtins/src/
        // logging_shims.rs) rather than real instance field slots. Real
        // FileHandler bytecode (loaded from java.base) is concrete, so
        // without this entry the default rule ran its REAL <init>()V /
        // <init>(String)V -- which try to actually open/lock a real log
        // file via NIO and throw NoSuchFileException -- instead of the
        // registered native. Keep in sync with vm_exec.rs's
        // invoke_on_class_shared_inner check_override entry for the same
        // triples; see
        // filehandler-noarg-ctor-handler-field-layout-gap-FIXED.md.
        || (class_name == "java/util/logging/FileHandler"
            && matches!(
                method_name,
                "<init>" | "publish" | "flush" | "close"
            ))
        || redefine_immune_synthetic_collection_native(class_name)
        || redefine_immune_vm_minted_carrier_native(class_name)
        || redefine_immune_thread_local_native(class_name)
        || redefine_immune_zip_file_native(class_name, method_name)
}

/// CratonVM implements these collections as small synthetic objects — a bucket
/// array plus a size, not the JDK's `table`/`root`/`head` field graph — and
/// every operation on them is a registered native. Their real JDK bodies can
/// therefore NEVER run correctly against an instance CratonVM built, whatever
/// the circumstances.
///
/// That makes them unconditionally immune: a redefinition drops native shadows
/// so an agent's woven bytecode can run, which is right for an ordinary class
/// and catastrophic here. `RedefineCollectionLayoutProbe` measured the damage
/// before this gate existed — **32 of 89 operations** changed behaviour after
/// redefining these classes with their OWN bytes, and the worst of them are
/// silent:
///
/// ```text
/// TreeMap.get               v7  -> null
/// TreeMap.containsKey       true -> false
/// ConcurrentHashMap.get     v7  -> null
/// ConcurrentHashMap.size    12  -> 0
/// ConcurrentHashMap.isEmpty false -> true
/// HashMap.keySet            [k0..k11] -> []
/// ```
///
/// The rest throw — `LinkedHashMap$Node.getKey` NoSuchMethodError,
/// `AnonymousObject$4 cannot be cast to Map$Entry`, `TreeSet` NPEs on a null
/// `this.m`. One `Mockito.mock()` anywhere in the process was enough to arm it,
/// which is how it reached Spring's AOT run: `AnnotationAttributes` extends
/// `LinkedHashMap`, and its `keySet()` NPE'd.
///
/// Unlike the StringBuilder list above, this is class-wide rather than
/// method-wise. There is no analogue of `length()`/`substring(int)` here — no
/// suite stubs a method on a mocked JDK collection, and the cost of being wrong
/// in that direction (one un-stubbed mock) is far below the cost of being wrong
/// in the other (silent data loss on every real collection in the process). If
/// a test ever does need to stub one, narrow this the way the builder list is
/// narrowed, and say which test.
fn redefine_immune_synthetic_collection_native(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/util/ArrayDeque"
            | "java/util/ArrayList"
            | "java/util/HashMap"
            | "java/util/HashSet"
            | "java/util/IdentityHashMap"
            | "java/util/LinkedHashMap"
            | "java/util/LinkedHashSet"
            | "java/util/LinkedList"
            | "java/util/TreeMap"
            | "java/util/TreeSet"
            | "java/util/concurrent/ConcurrentHashMap"
    )
}

/// A class the VM MINTED has no bytecode to yield to, in this or any image.
///
/// `cratonvm/internal/*` names a carrier this VM allocates and serves entirely
/// from registered natives -- `UnmodifiableList`, `UnmodifiableMap`, the
/// iterator and entry-set carriers, `MemorySegmentImpl`, `SystemLogger`. No
/// image contains a class file for any of them, so "drop the native and let the
/// woven bytecode run" cannot mean what it means for a real class: there is no
/// bytecode of theirs to run, and `find_method_recursive` walks past them to
/// whatever ANCESTOR declares the name -- in practice `java/lang/Object`.
///
/// Mockito's inline mock maker retransforms `java.lang.Object` whenever it mocks
/// a class (rather than an interface), which bumps Object's redefine generation
/// for the rest of the process. Without this arm, `vm_exec`'s
/// `native_shadow_dropped_by_redefine` then resolved
/// `cratonvm/internal/UnmodifiableList.equals` to `Object.equals`'s body, saw
/// `has_body && generation > 0`, and dropped the carrier's native -- so every
/// `Collections.unmodifiableList(..)` / `List.of(..)` /
/// `Collections.unmodifiableSet(..)` in the process compared by IDENTITY from
/// the first `mock()` onwards. Measured on `apps/probes/MinAssertRedefine.java`
/// (mock an abstract class, then compare):
///
/// ```text
/// [native-shadow] cratonvm/internal/UnmodifiableList.equals(Ljava/lang/Object;)Z
///                 dropped=true probe=Some((ClassId(0), true, 2)) immune=false
/// ```
///
/// and, one door up, AssertJ's `assertThat(list).isEqualTo(other)` failing with
/// `expected: "[x] (SingletonList@..)" but was: "[x] (UnmodifiableRandomAccessList@..)"`
/// while `list.equals(other)` on the same two objects answered `true` -- the
/// shape recorded for `LoggersEndpointTests` and
/// `CouchbaseAutoConfigurationTests` in the twelve-unclustered Spring residuals.
///
/// Class-wide and name-prefixed on purpose: the property is structural, not a
/// per-method judgement. A `cratonvm/internal/*` receiver never has a real body
/// under it, so there is nothing an agent could have woven into it and nothing
/// to narrow later. An agent that wants to intercept these collections mocks the
/// `java.util` class it sees, which is what the arm above covers.
fn redefine_immune_vm_minted_carrier_native(class_name: &str) -> bool {
    class_name.starts_with("cratonvm/internal/")
}

/// `ThreadLocal`'s values do not live where its real JDK body looks for them.
///
/// CratonVM serves `get`/`set`/`remove`/`initialValue`/`withInitial`/`<init>` on
/// these two classes from registered natives whose store is a Rust
/// thread-local keyed by the ThreadLocal's identity hash
/// (`TL_MAP` / `tl_with_initial_suppliers`, `native-builtins/src/
/// phases_early.rs`). The real `java.lang.ThreadLocal` bytecode reads
/// `Thread.threadLocals` — a `ThreadLocalMap` those natives never populate —
/// so it can only ever answer `null`, for every value the process has set.
///
/// Same shape as the synthetic-collection arm above, and the same trigger:
/// Mockito's inline mock maker instruments the target's whole superclass chain,
/// so `mock()` of ANY `ThreadLocal` subclass — `org.springframework.core.
/// NamedThreadLocal` is the one that found this — retransforms
/// `java.lang.ThreadLocal` itself, the suppress-native-shadow-on-redefine rule
/// fires, and every `ThreadLocal` in the process silently empties. Measured on
/// one binary with the mock target as the only variable
/// (`probes/ThreadLocalRetransformProbe.java`, `--dump-native-registry`):
///
/// ```text
///                                 mock a ThreadLocal subclass | mock any other class
///   ThreadLocal.get native calls                            4 |                  23
///   value set before the mock                            null |              "hello"
///   ThreadLocal.withInitial(() -> TRUE).get()            null |                true
///   probe                                        PROBE-FAIL 4 |           PROBE-OK
/// ```
///
/// Mockito's own internals are the first casualty, which is why the failure
/// reads as "Mockito broke" rather than "ThreadLocal broke":
/// `InlineDelegateByteBuddyMockMaker` holds `ThreadLocal.withInitial(() ->
/// false)` fields, and once those answer `null` the very next
/// `Boolean.booleanValue()` NPEs — inside mock creation, so EVERY subsequent
/// `mock()` in the JVM fails too (89 of 728 in the sweep that found this).
///
/// Class-wide rather than method-wise, for the collection arm's reason: no
/// suite stubs a method on a mocked `ThreadLocal`, and one un-stubbed mock
/// costs far less than silent data loss on every ThreadLocal in the process.
/// A subclass that overrides `initialValue()` is unaffected either way — that
/// override is the subclass's own bytecode and dispatch resolves to it before
/// reaching this gate.
///
/// # `--jdk-only` reads this table too (G5-1, 2026-08-16)
///
/// It is easy to read the paragraphs above as a `Compatible`-mode story. It is
/// not. `register_thread_local_natives` (`native-builtins/src/phases_early.rs`)
/// wraps all six registrations in `set_category(NativeKind::Intrinsic)`, and
/// `resolve_native_dispatch_wave1` (`vm/src/vm/vm_exec.rs`) admits an
/// `Intrinsic` over concrete bytecode under `--jdk-only` as well — §1.4's
/// reviewed exception. So under `--jdk-only`, exactly as under `Compatible`,
/// `Thread.threadLocals` and `Thread.inheritableThreadLocals` are never
/// written by anything in the process.
///
/// That is not only a storage detail. It silently disables three real-JDK
/// behaviours that live in `java.lang.Thread`'s and `java.lang.ThreadLocal`'s
/// own bytecode and have no analogue in `TL_MAP`:
///
/// * the construction-time inheritance copy at pc 175..201 of the master
///   `Thread(ThreadGroup, String, int, Runnable, long)` constructor — its
///   `ifnull` at pc 182 always takes the skip branch;
/// * the `characteristics & 4` opt-out that `Thread(g, r, n, ss, false)` sets;
/// * `InheritableThreadLocal.childValue(T)` overrides, which the JDK applies
///   inside `createInheritedMap`.
///
/// All three are MEASURED divergences on Temurin 25.0.3+9 and are written up,
/// with the demotion nomination that would restore them, in
/// `docs/known-issues/jdk-only/G5-1-inheritable-threadlocal-captures-at-
/// construction-20260816.md`. Do not "fix" the ITL timing anywhere downstream
/// without reading §5 there first: the workaround it describes lives in
/// `native-builtins/src/lang_system.rs::native_thread_start0`, not here.
fn redefine_immune_thread_local_native(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/lang/ThreadLocal" | "java/lang/InheritableThreadLocal"
    )
}

pub(crate) fn should_force_registered_native_over_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    should_force_registered_native_over_bytecode_precomputed(
        shared,
        force_native_over_real_jdk_bytecode_memoized(class_name, method_name, method_descriptor),
        class_name,
        method_name,
        method_descriptor,
    )
}

/// Memoizing wrapper around [`force_native_over_real_jdk_bytecode`].
///
/// That function is a pure, ~55-branch sequential scan over hardcoded
/// (class, method, descriptor) triples with no side effects and no
/// dependency on mutable VM state -- its result for a given triple never
/// changes for the lifetime of the process. `CachedBytecodeMethod
/// ::force_native_cache` already memoizes it once per warm bytecode-PC
/// invoke-cache entry, but every OTHER call path that reaches
/// `should_force_registered_native_over_bytecode` -- reflective
/// `Method.invoke()` dispatch (which has no bytecode PC to key an
/// invoke-cache entry on), megamorphic/polymorphic call sites that never
/// settle on one cached target, `invokespecial`, and interface-default
/// dispatch -- re-ran the full scan on every single call with no
/// memoization at all. Profiling `BeanRegistrationsAotContributionTests`
/// (~54 min vs HotSpot's 13s for the same test, see
/// CRATONVM-SPRING-GENUINE-BUGLIST's AOT cluster
/// section) found exactly this: `intercept_force_registered_native` ->
/// `should_force_registered_native_over_bytecode` ->
/// `force_native_over_real_jdk_bytecode` live at the top of repeated gdb
/// stack samples, reached through deep `try_lambda_dispatch` recursion
/// driven by Mockito's constructor-mock listener dispatch (reflective
/// `Method.invoke()` on many distinct generated classes, so per-callsite
/// caching never warms up). A global cache keyed by the exact same 3
/// inputs is safe by construction -- the wrapped function reads no state
/// beyond its own arguments, so there is nothing to invalidate.
pub(super) fn force_native_over_real_jdk_bytecode_memoized(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    type Key = (Box<str>, Box<str>, Box<str>);
    static CACHE: OnceLock<Mutex<rustc_hash::FxHashMap<Key, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(rustc_hash::FxHashMap::default()));
    let key: Key = (
        class_name.into(),
        method_name.into(),
        method_descriptor.into(),
    );
    if let Some(&v) = cache.lock().get(&key) {
        return v;
    }
    let v = force_native_over_real_jdk_bytecode(class_name, method_name, method_descriptor);
    cache.lock().insert(key, v);
    v
}

/// Same decision as [`should_force_registered_native_over_bytecode`], but
/// takes the pure/deterministic `force_native_over_real_jdk_bytecode` result
/// as a precomputed input rather than recomputing it. Lets a cached-dispatch
/// call site (which can memoize that ~55-branch check once per invoke-cache
/// entry, see `CachedBytecodeMethod::force_native_cache`) skip straight to
/// the cheap, mutable-state-dependent redefine check.
pub(super) fn should_force_registered_native_over_bytecode_precomputed(
    shared: &SharedVm,
    force_native: bool,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    force_native
        && (!native_shadow_suppressed_by_redefine(shared, class_name)
            || redefine_immune_forced_native(class_name, method_name, method_descriptor))
}

/// [`should_force_registered_native_over_bytecode_precomputed`] with the
/// receiver-aware ZIP waiver applied to the immunity term.
///
/// This is THE gate that decides for `java/util/jar/JarFile.close()V`: the
/// method is a force-native entry, so it never reaches the shadow-drop gates in
/// `invoke_or_native` and `try_stackless_invoke` at all. Patching those two and
/// not this one moves the `[native-shadow]` diagnostic to `dropped=true` and
/// changes nothing about which body runs -- measured 2026-09-10, `mock(JarFile
/// .class).close()` still recorded zero invocations.
///
/// The `real_http_url_connection_native` arm a few hundred lines below is the
/// same idea reached independently: it tells "a genuinely real carrier" from
/// "a mock or synthetic carrier" by checking the receiver's field 0, because an
/// Objenesis-constructed mock never ran a constructor. This asks the archive
/// tables the same question, which is exact rather than a proxy.
pub(super) fn should_force_registered_native_over_bytecode_precomputed_for_receiver(
    shared: &SharedVm,
    force_native: bool,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    force_native
        && (!native_shadow_suppressed_by_redefine(shared, class_name)
            || redefine_immune_forced_native_for_receiver(
                shared,
                class_name,
                method_name,
                method_descriptor,
                receiver,
            ))
}

/// [`should_force_registered_native_over_bytecode`], receiver-aware.
pub(crate) fn should_force_registered_native_over_bytecode_for_receiver(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    should_force_registered_native_over_bytecode_precomputed_for_receiver(
        shared,
        force_native_over_real_jdk_bytecode_memoized(class_name, method_name, method_descriptor),
        class_name,
        method_name,
        method_descriptor,
        receiver,
    )
}

/// The receiver of an instance call, for the gates above. `None` for a static
/// call, a null receiver, or a primitive first argument.
pub(super) fn receiver_of(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    }
}

/// Route a force-native interception through §7 policy, and count it.
///
/// # The gap this closes
///
/// [`intercept_force_registered_native`] and its cached twin are reached from
/// **seven** call sites across `dispatch_static`, `dispatch_virtual` and
/// `invoke`, and both used to end in a bare `safe_native_call` on a callback
/// from `NativeMethodRegistry::find`. No `dispatch_policy`, no
/// `resolve_native_dispatch_wave1`, no `record_invocation`. Their entire
/// purpose is to make a registered native beat *concrete real-JDK bytecode*,
/// which is precisely the inversion §1.4 forbids under `JdkOnly` — so under
/// `--jdk-only` these were seven unguarded holes in contract §11's *"every
/// strict-mode native dispatch"*, and in `Compatible` they were seven
/// dispatches missing from the §4 census. Every sibling dispatch route
/// (`invoke_or_native`, `try_stackless_invoke` steps 1 and 6,
/// `invoke_on_class_shared_inner`) was routed in wave 1; these two were not.
///
/// # Why `bytecode_available: true`
///
/// Unlike `resolve_step1_native`, which runs before any method resolution and
/// passes `false` because it genuinely does not know, these sites are only
/// reached *because* `force_native_over_real_jdk_bytecode` said this triple's
/// real bytecode must lose. Concrete bytecode existing is the premise of the
/// call, so `true` is the honest input to §7 step 3 — and it is what makes a
/// strict run fall through to that bytecode instead of running the shadow.
///
/// # `Compatible` is bit-for-bit unchanged
///
/// `compat_native_wins` is `true` — exactly the unconditional "a registered
/// native wins here" the `find` call encoded — and in `Compatible` mode
/// `resolve_native_dispatch_wave1` is a pure function of that boolean. The
/// added cost is one relaxed `fetch_add` for the census.
///
/// # The three answers
///
/// * `Ok(Some(cb))` — dispatch it, and the invocation has been counted.
/// * `Ok(None)` — nothing registered, or §7 step 3 sent a `Bridge` to the real
///   bytecode. The interceptor declines, and the call proceeds to that
///   bytecode. The shadow attempt is already recorded by the resolver.
/// * `Err(violation)` — §1.3, a `SyntheticStub` under `--jdk-only`. Raised
///   rather than declined, matching `invoke_on_class_shared_inner`, so it is
///   counted as a `SyntheticNativeInvocation`. Swallowing it would run the real
///   bytecode quietly and leave a strict run reporting zero synthetic-stub
///   invocations for a call that was one — the exact false-green contract §11
///   must be immune to.
#[inline]
fn admit_forced_native(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> Result<Option<cratonvm_native_api::NativeCallback>, cratonvm_types::error::JdkOnlyViolation> {
    let Some(id) =
        shared
            .natives
            .native_methods
            .resolve_id(class_name, method_name, method_descriptor)
    else {
        return Ok(None);
    };
    admit_forced_native_id(shared, id, class_name, method_name, method_descriptor)
}

/// [`admit_forced_native`] for a caller that already holds the resolved
/// [`cratonvm_native_api::NativeMethodId`].
///
/// The cached interceptor gets its id from the call site's generation-keyed
/// `NativeCallSite` memo, which exists because the plain
/// `NativeMethodRegistry::find` it replaced was measured as the #2 hottest
/// symbol (~7% of samples) on `TestResponsePerformance`. Routing that path
/// through the string-hashing sibling would hand that back; taking the id
/// keeps the warm cost at two array indexes, the policy call, and one relaxed
/// `fetch_add`.
#[inline]
fn admit_forced_native_id(
    shared: &SharedVm,
    id: cratonvm_native_api::NativeMethodId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> Result<Option<cratonvm_native_api::NativeCallback>, cratonvm_types::error::JdkOnlyViolation> {
    let registry = &shared.natives.native_methods;
    let Some(callback) = registry.callback_of(id) else {
        return Ok(None);
    };
    let kind = registry
        .kind_of_id(id)
        .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
    match crate::vm::resolve_native_dispatch_wave1(
        crate::vm::DispatchDoor::ForceIntercept,
        crate::vm::dispatch_policy(shared),
        class_name,
        method_name,
        method_descriptor,
        Some((callback, kind)),
        true,
        true,
    ) {
        // §1.3 — a `SyntheticStub` may not be invoked under `--jdk-only`.
        // Raised rather than silently declined, matching
        // `invoke_on_class_shared_inner`: the violation is counted as a
        // `SyntheticNativeInvocation` by the caller that turns it into
        // `VmError::JdkOnly`, and swallowing it here would drop that census
        // entry while quietly running the real bytecode — a strict run would
        // then report zero synthetic-stub invocations for a call that was one.
        Some(crate::vm::DispatchDecision::Reject(violation)) => Err(violation),
        Some(decision) => match decision.native_callback() {
            Some(admitted) => {
                // Counted at the point of actual dispatch, matching every
                // other route: the caller calls `safe_native_call` next.
                registry.record_invocation(id);
                Ok(Some(admitted))
            }
            None => Ok(None),
        },
        // §7 step 3 under `JdkOnly`: concrete bytecode beats this bridge. The
        // shadow attempt was already recorded by the resolver.
        None => Ok(None),
    }
}

// JDK-ONLY-WAVE2, RETIRED 2026-08-06. `THREADPOOL_EXECUTE_RECEIVER_SHAPE_SITES`
// (the census of the eight dispatch sites that carried the
// `ThreadPoolExecutor.execute` receiver-shape probe) and
// `threadpool_executor_has_real_workers` (the probe itself) lived here.
//
// All eight are gone, together with the ninth, receiver-blind arm in
// `force_native_over_real_jdk_bytecode` that they existed to override. What
// replaced them: `native_es_execute` is tagged `NativeKind::SyntheticStub`
// (`native-builtins/src/util_concurrent_ext.rs`) and
// `java/util/concurrent/ThreadPoolExecutor` is on
// `real_protected_stub_class_common`'s allow-list, so the one centralised
// arbitration yields the stub to the real `execute()` body class-scoped, for
// every receiver, on both the warm and the cold dispatch path.
//
// That is only sound because the per-INSTANCE distinction was eliminated
// first, by L10 (2026-08-06) and at REGISTRATION rather than in dispatch:
// `NativeMethodRegistry::register` drops every `Executors` pool factory when
// `drop_real_layout_synthetic` is set, so on a real-JDK image the real
// `java.util.concurrent.Executors` bytecode builds every executor and CratonVM
// has no code path that can mint a fabricated one. The sites are deletable
// because the fabricated receiver CANNOT EXIST -- a statement about the code,
// not the `false=0` reading two workloads produced, which L10 measured as
// already identical before it landed. `every_threadpool_receiver_shape_site_is_gone` below is the gate that
// keeps a copy from growing back.
//
// `native-builtins`' own `executor_has_real_workers` (and the deliberately
// separate twin in `native-collections`) is NOT part of this: it is defence in
// depth *inside the callee*, re-checking and redirecting a genuinely-real
// receiver that reached the native anyway. It stays, and is listed here only so
// a grep does not mistake it for a dispatch site.

// ---------------------------------------------------------------------------
// Per-call-site shape of the cached interception chain
// ---------------------------------------------------------------------------

/// This triple could reach the `ClassLoader` null-resource re-target.
pub(super) const INTERCEPT_SHAPE_CLASSLOADER_RESOURCE: u8 = 1 << 0;
/// This triple could reach the `java/lang/Class` reflection re-target.
pub(super) const INTERCEPT_SHAPE_CLASS_REFLECTION: u8 = 1 << 1;
/// This triple could reach the real-`HttpURLConnection` carrier exemption.
pub(super) const INTERCEPT_SHAPE_HTTP_CARRIER: u8 = 1 << 2;
/// One of the three name-matched intercepts in the cached virtual
/// dispatcher (`ClassLoader.setDefaultAssertionStatus`, the surefire
/// `LazyLauncher.discover` native, the reflective `Method.invoke` /
/// `Constructor.newInstance` override). Consumed only by the invoke fast
/// door, which declines any method with a non-zero shape.
pub(super) const INTERCEPT_SHAPE_NAMED: u8 = 1 << 3;

/// Classify a call site's triple against the three special-case arms of
/// [`intercept_force_registered_native_cached`], once.
///
/// Pure: it reads the triple and nothing else — no VM, no policy, no receiver,
/// no arguments — which is what makes it safe to memoize in
/// `CachedBytecodeMethod::intercept_shape_cache`. See that field's doc for the
/// measurement.
///
/// **Keep this in step with the arms it gates.** A new name-keyed arm that
/// forgets to set a bit here is not slow, it is SKIPPED — a correctness bug.
/// `intercept_shape_agrees_with_the_arms_it_gates` in this module's tests is
/// what fails when they drift.
pub(super) fn intercept_shape_of(class_name: &str, method_name: &str, descriptor: &str) -> u8 {
    let mut shape = 0u8;
    if classloader_resource_shape(method_name, descriptor) {
        shape |= INTERCEPT_SHAPE_CLASSLOADER_RESOURCE;
    }
    if class_reflection_shape(method_name, descriptor) {
        shape |= INTERCEPT_SHAPE_CLASS_REFLECTION;
    }
    if http_carrier_declaring_class(class_name) {
        shape |= INTERCEPT_SHAPE_HTTP_CARRIER;
    }
    if named_intercept_shape(class_name, method_name, descriptor) {
        shape |= INTERCEPT_SHAPE_NAMED;
    }
    shape
}

/// The name triples `intercept_classloader_set_default_assertion_status`,
/// `surefire_lazy_launcher_discover_native` and
/// `native_override_for_cached_reflect_invoke` match on.
fn named_intercept_shape(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    matches!(
        (class_name, method_name, descriptor),
        (_, "setDefaultAssertionStatus", "(Z)V")
            | (
                _,
                "discover",
                "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;"
            )
            | (
                "java/lang/reflect/Method",
                "invoke",
                "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
            )
            | (
                "java/lang/reflect/Constructor",
                "newInstance",
                "([Ljava/lang/Object;)Ljava/lang/Object;"
            )
    )
}

/// The name-keyed half of the `ClassLoader` null-resource re-target.
fn classloader_resource_shape(method_name: &str, descriptor: &str) -> bool {
    matches!(
        (method_name, descriptor),
        ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
            | (
                "getResources",
                "(Ljava/lang/String;)Ljava/util/Enumeration;"
            )
            | (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;"
            )
    )
}

/// The name-keyed half of the `java/lang/Class` reflection re-target.
fn class_reflection_shape(method_name: &str, descriptor: &str) -> bool {
    matches!(
        (method_name, descriptor),
        ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
            | ("isArray", "()Z")
            | ("getComponentType", "()Ljava/lang/Class;")
            | ("componentType", "()Ljava/lang/Class;")
    )
}

/// The whole `class_name` gate of [`real_http_url_connection_native`], factored
/// out so the memo and the arm cannot disagree about it.
fn http_carrier_declaring_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/net/URLConnection"
            | "java/net/HttpURLConnection"
            | "javax/net/ssl/HttpsURLConnection"
            | "sun/net/www/protocol/http/HttpURLConnection"
            | "sun/net/www/protocol/https/HttpsURLConnectionImpl"
    )
}

/// CratonVM's own HTTP carrier classes — the concrete classes its
/// `URL.openConnection()` hands back, and the ones
/// `register_http_url_connection_real` registers natives on. Matched EXACTLY
/// (not by subtype): a user subclass such as
/// `SimpleClientHttpRequestFactoryTests$TestHttpURLConnection` has real
/// bytecode of its own and must keep running it.
const CRATONVM_HTTP_CARRIER_CLASSES: [&str; 4] = [
    "java/net/HttpURLConnection",
    "sun/net/www/protocol/http/HttpURLConnection",
    "sun/net/www/protocol/https/HttpsURLConnectionImpl",
    "javax/net/ssl/HttpsURLConnection",
];

/// Resolve the registered native for a call landing on a genuinely real,
/// `URL.openConnection()`-constructed CratonVM HTTP carrier — the one case
/// where the native must fire even though the class counts as "redefined"
/// somewhere in the process.
///
/// Why the exemption exists: Mockito's mock makers trip the class-wide
/// `class_redefine_generation` counter for EVERY instance of
/// `java/net/HttpURLConnection`, mock or not, for the rest of the process.
/// Without this, `should_force_registered_native_over_bytecode` cedes to the
/// real-JDK bytecode for a real, non-mock connection too — observed as
/// `getResponseCode()` returning 0 and `addRequestProperty`/`getHeaderField`
/// silently no-op'ing, because CratonVM's carrier keeps its request/response
/// state in native side tables the real JDK bytecode never touches. That is
/// `SimpleClientHttpRequestFactoryTests.interceptor()` losing the
/// interceptor's added header.
///
/// Keyed on the RECEIVER, not on `class_name`, and that is load-bearing twice:
///
///  * `class_name` is the resolved method's declaring class, so the very same
///    `connection.addRequestProperty(...)` call site in
///    `SimpleClientHttpRequest.addHeaders` reports `java/net/HttpURLConnection`
///    at first and `java/net/URLConnection` once an unrelated
///    `Mockito.mock(HttpURLConnection.class)` has re-resolved it. The old
///    `class_name == "java/net/HttpURLConnection"` test silently stopped
///    matching at that point — and it was missing from the cached/hot twin
///    entirely, so it also stopped applying as soon as a call site warmed up.
///  * a Mockito mock is Objenesis-constructed (no constructor ever runs), so
///    its inherited `URLConnection.url` field 0 stays unset, while a real
///    carrier's is always populated. The field-0 test therefore never fires
///    for a mock, and mocking `HttpURLConnection` still routes through
///    Mockito's advice for stubbing and verification.
///
/// Returns the callback to force, or `None` to let normal dispatch decide.
pub(super) fn real_http_url_connection_native(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<cratonvm_native_api::registry::NativeCallback> {
    // Cheap gate first: only the connection hierarchy can reach the exemption.
    // Shared with `intercept_shape_of`, which memoizes this exact question per
    // call site, so the two cannot disagree about which classes qualify.
    if !http_carrier_declaring_class(class_name) {
        return None;
    }
    let Some(Value::Object(Some(receiver))) = args.first() else {
        return None;
    };
    // Objenesis-constructed mock => field 0 unset => not a real carrier.
    if !matches!(
        shared.mem.heap.get_field(*receiver, 0),
        Value::Object(Some(_))
    ) {
        return None;
    }
    let receiver_cid = shared.mem.heap.class_id_of(*receiver);
    let receiver_name = shared
        .classes
        .class_manager
        .read()
        .get_class(receiver_cid)
        .map(|class| class.name.to_string())?;
    if !CRATONVM_HTTP_CARRIER_CLASSES.contains(&receiver_name.as_str()) {
        return None;
    }
    shared
        .natives
        .native_methods
        .find(&receiver_name, method_name, method_descriptor)
        .or_else(|| {
            shared.natives.native_methods.find(
                "java/net/HttpURLConnection",
                method_name,
                method_descriptor,
            )
        })
}

pub(super) fn intercept_force_registered_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    // FileChannel.open() invokes FileSystemProvider.newFileChannel through a
    // default-provider receiver (WindowsFileSystemProvider on this host),
    // while the fd-backed native is registered on the JDK base class. Route
    // the forced call to that base registration explicitly so a cached
    // runtime receiver name cannot bypass it and run the JDK's deliberate
    // UnsupportedOperationException stub.
    //
    // `createSymbolicLink`/`createLink`/`readSymbolicLink` ride the same route
    // for the same reason — see `is_file_system_provider_link_native_override`.
    if (method_name == "newFileChannel"
        && method_descriptor
            == "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;"
        && matches!(
            class_name,
            "java/nio/file/spi/FileSystemProvider"
                | "sun/nio/fs/WindowsFileSystemProvider"
                | "sun/nio/fs/UnixFileSystemProvider"
        ))
        || is_file_system_provider_link_native_override(
            class_name,
            method_name,
            method_descriptor,
        )
    {
        let cb = shared.natives.native_methods.find(
            "java/nio/file/spi/FileSystemProvider",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // The resource-name argument is specified to be non-null for every
    // ClassLoader resource accessor.  A virtual call whose constant-pool
    // owner is ClassLoader can resolve to an inherited cached method on a
    // custom loader, so the generic force-native lookup below sees the custom
    // class name and misses the callback registered on ClassLoader.  Route
    // only the null-argument contract through that base callback before
    // method-cache dispatch; normal non-null calls retain the custom loader's
    // virtual implementation.
    if args.len() == 2
        && matches!(args.get(1), Some(Value::Object(None)))
        && matches!(
            (method_name, method_descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        let cb = shared.natives.native_methods.find(
            "java/lang/ClassLoader",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // `Class.getClassLoader()` is a concrete JDK method, but Class mirrors in
    // this VM use an internal layout and their real `classLoader` field can be
    // a stale non-loader object.  Dispatch by the receiver's *runtime* class
    // before the normal declaring-class gate: JDK calls reached through an
    // inherited/cached method reference can otherwise bypass the static
    // allowlist and hand ServiceLoader a String as its loader.
    if method_name == "getClassLoader"
        && method_descriptor == "()Ljava/lang/ClassLoader;"
        && matches!(
            args.first(),
            Some(Value::Object(Some(receiver))) if {
                let receiver_cid = shared.mem.heap.class_id_of(*receiver);
                shared
                    .classes.class_manager
                    .read()
                    .get_class(receiver_cid)
                    .map(|class| &*class.name == "java/lang/Class")
                    .unwrap_or(false)
            }
        )
    {
        // Fully-constant triple: memoized in a file-local cell rather than
        // re-hashing three literals on every call (native-dispatch-memoization
        // §3 Step 1, B1). The memo is keyed on the registry generation, so a
        // native registered later is still picked up and a negative result
        // self-heals — unlike a `OnceLock`.
        //
        // ONE STATIC, ONE TRIPLE. The generation is the *only* key: the triple
        // is not re-verified on a warm hit (re-hashing it is the cost this
        // exists to remove), so a cell reached with a second triple can redeem
        // the first's memoized negative and silently report "no native" for a
        // registered one. This cell is reached from exactly one call, with
        // three string literals.
        static NCS_CLASS_GET_CLASSLOADER: cratonvm_native_api::NativeCallSite =
            cratonvm_native_api::NativeCallSite::new();
        let callback = NCS_CLASS_GET_CLASSLOADER.callback(
            &shared.natives.native_methods,
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // These concrete Class methods read VM-private mirror fields in JDK 25.
    // Resolve by the receiver's runtime class so inherited or cached method
    // references cannot bypass CratonVM's class-id-backed native methods.
    if matches!(
        (method_name, method_descriptor),
        ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
            | ("isArray", "()Z")
            | ("getComponentType", "()Ljava/lang/Class;")
            | ("componentType", "()Ljava/lang/Class;")
    ) && matches!(
        args.first(),
        Some(Value::Object(Some(receiver))) if {
            let receiver_cid = shared.mem.heap.class_id_of(*receiver);
            shared
                .classes.class_manager
                .read()
                .get_class(receiver_cid)
                .map(|class| &*class.name == "java/lang/Class")
                .unwrap_or(false)
        }
    ) {
        let callback = shared.natives.native_methods.find(
            "java/lang/Class",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // `java/net/HttpURLConnection`'s real-carrier natives (connect,
    // getResponseCode, getHeaderField, addRequestProperty, ...) must keep
    // firing for a genuinely real, `URL.openConnection()`-constructed carrier
    // (its real inherited `URLConnection.url` field 0 populated) even after
    // ANY instance of this class has been JVMTI-redefined elsewhere in the
    // process — e.g. a completely unrelated `Mockito.mock(HttpURLConnection
    // .class)` call. Mockito's default "inline" mock maker redefines the
    // TARGET CLASS's bytecode IN PLACE rather than subclassing it, so the
    // class-wide `class_redefine_generation` counter trips permanently for
    // EVERY instance of the class, mock or not, for the rest of the process.
    // Without this, `should_force_registered_native_over_bytecode`'s redefine
    // check below cedes to the now-Mockito-woven bytecode for a real,
    // non-mock connection too — observed as `getResponseCode()` silently
    // returning 0 and `getHeaderField`/`addRequestProperty` silently no-op'ing
    // instead of touching the real request/response, so
    // `SimpleClientHttpRequestFactoryTests.interceptor()` failed first with
    // "Status code '0' should be a three-digit positive integer" and then
    // (once getResponseCode alone was exempted) with the interceptor's added
    // header missing from the echoed response, simply because an EARLIER,
    // unrelated test method in the same JVM mocked HttpURLConnection.
    //
    // A Mockito mock itself is Objenesis-constructed (no constructor ever
    // runs), so its field 0 stays null — checking for a non-null field 0
    // cheaply distinguishes "genuinely real carrier" from "mock or synthetic
    // carrier" without invoking `toExternalForm`, and this exemption never
    // fires for an actual mock (whose field 0 is always null), so mocking
    // HttpURLConnection still correctly routes through Mockito's advice for
    // stubbing/verification. Deliberately not narrowed to a specific method
    // allowlist: any native registered on this class for a real carrier is
    // safe to force, since the receiver check alone already gates out mocks.
    if let Some(callback) =
        real_http_url_connection_native(shared, class_name, method_name, method_descriptor, args)
    {
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native: class={} method={}{} \
             force={}",
            class_name,
            method_name,
            method_descriptor,
            force_native_over_real_jdk_bytecode(class_name, method_name, method_descriptor),
        );
    }
    // A JVMTI agent that redefined this class (e.g. a Mockito inline mock)
    // makes its woven bytecode authoritative — cede to it instead of forcing
    // the native, so the instrumentation advice runs. Reflection-metadata
    // natives are exempt (see `redefine_immune_reflection_native`): the real
    // bytecode cannot reproduce them under CratonVM.
    if !should_force_registered_native_over_bytecode_for_receiver(
        shared,
        class_name,
        method_name,
        method_descriptor,
        receiver_of(args),
    ) {
        return None;
    }
    // (Receiver-shape probe deleted 2026-08-06 — see
    // `force_native_over_real_jdk_bytecode`. The ninth, receiver-blind arm this
    // one existed to undo went with it, so there is nothing left to undo.)
    // §7 routing. This was a bare `find`, so the dispatch below ran with no
    // policy check and no census count — see `admit_forced_native` for why
    // that made this one of seven unguarded strict-mode holes. `None` here
    // means the policy refused, and declining to intercept hands the call to
    // the real bytecode this site exists to override, which is §7 step 3's
    // answer.
    let cb = match admit_forced_native(shared, class_name, method_name, method_descriptor) {
        Ok(Some(cb)) => cb,
        Ok(None) => return None,
        Err(violation) => {
            return Some(Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                violation,
            ))));
        }
    };
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!("[ccs-probe] intercept_force_registered_native: dispatching native callback");
    }
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// Perf variant of [`intercept_force_registered_native`] for the cached/hot
/// dispatch paths (`execute_invokevirtual_cached`, `execute_invokestatic_cached`)
/// that already hold an `Arc<CachedBytecodeMethod>` for this callsite. The
/// original re-evaluated `force_native_over_real_jdk_bytecode`'s ~55-branch
/// sequential string-comparison gauntlet from scratch on *every single*
/// cached-invoke hit -- this was independently identified as a real
/// interpreter-throughput bottleneck (~51% of all executed instructions on
/// method-call-heavy workloads, see `docs/known-issues/tomcat-08-07/
/// silent-hang-no-signature-cluster.md`) and reproduced live via `perf`/`gdb`
/// during the `ClientHttpConnectorTests` investigation (2026-07-15): one
/// interpreter thread pegged at ~100% CPU for 25+ seconds cycling through
/// this exact call chain while executing a tight Java-level spin/poll loop
/// typical of Reactor/Netty/Jetty's lock-free scheduling. This variant reads
/// `cached.force_native_cache`, computing the pure part exactly once per
/// invoke-cache entry (memoized `OnceLock`, shared via the entry's `Arc`)
/// instead of on every hit; the mutable-state-dependent redefine check is
/// still re-evaluated every call (cheap, and must stay live).
pub(super) fn intercept_force_registered_native_cached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: &CachedBytecodeMethod,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let class_name = cached.class_name.as_ref();
    let method_name = cached.method_name.as_ref();
    let method_descriptor = cached.method_descriptor.as_ref();
    // Which of the three special-case arms below this triple can reach at all,
    // computed once per call site. Every one of their name/class keys is a
    // function of the triple, and the triple is fixed for this entry — so they
    // were per-call-site constants being re-derived on every cached-invoke hit.
    // `real_http_url_connection_native`'s five-literal `class_name` gate alone
    // measured 1.50% of the interpreted-invoke arm on a probe whose only call
    // is `int callee(int)`. The argument/receiver halves are NOT memoized and
    // are still evaluated in full below. See
    // `CachedBytecodeMethod::intercept_shape_cache`.
    //
    // Deliberately NOT an early `return` on `shape == 0` into a shared tail
    // function: that WAS built and measured (2026-08-11), and the function
    // split cost more than the three bit tests it skipped — entry + tail
    // 1.05% + 1.12% against a single 1.44% function before, with the string
    // work already removed from both. The bits guard the arms where they
    // stand; falling through them IS the fast path.
    let shape = *cached
        .intercept_shape_cache
        .get_or_init(|| intercept_shape_of(class_name, method_name, method_descriptor));
    // Keep the cached path aligned with the uncached null-resource contract
    // above.  The cache is keyed by the resolved custom-loader method, while
    // the implementation callback is deliberately registered on ClassLoader.
    if shape & INTERCEPT_SHAPE_CLASSLOADER_RESOURCE != 0
        && args.len() == 2
        && matches!(args.get(1), Some(Value::Object(None)))
    {
        let cb = shared.natives.native_methods.find(
            "java/lang/ClassLoader",
            method_name,
            method_descriptor,
        )?;
        let ret_type = crate::jit::return_type(method_descriptor);
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    if shape & INTERCEPT_SHAPE_CLASS_REFLECTION != 0
        && matches!(
            args.first(),
            Some(Value::Object(Some(receiver))) if {
                let receiver_cid = shared.mem.heap.class_id_of(*receiver);
                shared
                    .classes.class_manager
                    .read()
                    .get_class(receiver_cid)
                    .map(|class| &*class.name == "java/lang/Class")
                    .unwrap_or(false)
            }
        )
    {
        let callback = shared.natives.native_methods.find(
            "java/lang/Class",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // Same real-carrier exemption the uncached twin applies (see
    // `real_http_url_connection_native`). This path used to omit it entirely,
    // so the exemption held only until a call site warmed into the invoke
    // cache and then silently stopped applying — one
    // `Mockito.mock(HttpURLConnection.class)` anywhere in the process then
    // permanently broke every genuinely real connection's
    // `addRequestProperty`/`getResponseCode`/`getHeaderField`.
    if shape & INTERCEPT_SHAPE_HTTP_CARRIER != 0 {
        if let Some(callback) = real_http_url_connection_native(
            shared,
            class_name,
            method_name,
            method_descriptor,
            args,
        ) {
            return Some((|| {
                let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
                if let Some(value) = result {
                    push_invoke_return_value(
                        &mut thread.frames[frame_idx].stack,
                        coerce_value_for_return(value, crate::jit::return_type(method_descriptor)),
                    )?;
                    crate::vm::native_return_pushed_to_stack(shared, thread);
                }
                Ok(CachedCallResult::Handled)
            })());
        }
    }
    let force_native = *cached.force_native_cache.get_or_init(|| {
        force_native_over_real_jdk_bytecode(class_name, method_name, method_descriptor)
    });
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native_cached: class={} method={}{} \
             force={}",
            class_name, method_name, method_descriptor, force_native,
        );
    }
    // A JVMTI agent that redefined this class (e.g. a Mockito inline mock)
    // makes its woven bytecode authoritative — cede to it instead of forcing
    // the native, so the instrumentation advice runs. Reflection-metadata
    // natives are exempt (see `redefine_immune_reflection_native`): the real
    // bytecode cannot reproduce them under CratonVM.
    if !should_force_registered_native_over_bytecode_precomputed_for_receiver(
        shared,
        force_native,
        class_name,
        method_name,
        method_descriptor,
        receiver_of(args),
    ) {
        return None;
    }
    // (Receiver-shape probe deleted 2026-08-06 — see
    // `force_native_over_real_jdk_bytecode`. The ninth, receiver-blind arm this
    // one existed to undo went with it, so there is nothing left to undo.)
    // Site A1 of `native-dispatch-memoization.md`
    // §3 Step 2. Perf (2026-07-19, TestResponsePerformance residual): memoize
    // the resolved callback per invoke-cache entry, same shape as
    // `force_native_cache` above -- `NativeMethodRegistry::find` was the #2
    // hottest symbol (~7% of samples) on that benchmark.
    //
    // This was a `OnceLock<Option<NativeCallback>>` and is now a
    // generation-keyed `NativeCallSite`, which also FIXES A LATENT BUG: the
    // `OnceLock` memoized a *negative* permanently, on the argument that
    // native registration is immutable after boot. That holds for the steady
    // state but not for boot itself, nor for `alias_class` / the lazy
    // `register_*` passes that run after the first bytecode executes -- a
    // native registered by a later pass was invisible here forever, while
    // dispatching fine through `find`. The generation check re-resolves
    // exactly when a new slot is appended.
    //
    // ONE CELL, ONE TRIPLE: `class_name`/`method_name`/`method_descriptor` are
    // `cached.{class,method}_name` / `cached.method_descriptor` verbatim (bound
    // at the top of this function), so this cell only ever sees this entry's
    // own triple. The `java/lang/ClassLoader` re-target earlier in this
    // function deliberately stays on plain `find` for that reason.
    //
    // §7 routing (2026-08-04). This ended in a bare `safe_native_call` on the
    // memoized callback — no `dispatch_policy`, no
    // `resolve_native_dispatch_wave1`, no census count — which made it one of
    // seven unguarded strict-mode holes; see `admit_forced_native`. The memo
    // is kept: `resolve` hands back the `NativeMethodId` without re-hashing,
    // and `admit_forced_native_id` takes it from there, so the perf argument
    // above survives intact.
    let id = cached.native_call_site().resolve(
        &shared.natives.native_methods,
        class_name,
        method_name,
        method_descriptor,
    )?;
    let cb = match admit_forced_native_id(shared, id, class_name, method_name, method_descriptor) {
        Ok(Some(cb)) => cb,
        Ok(None) => return None,
        Err(violation) => {
            return Some(Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                violation,
            ))));
        }
    };
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native_cached: dispatching native callback"
        );
    }
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

#[inline]
pub(super) fn intercept_jython_pyjavatype_findattr_ex(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special {
        return None;
    }
    if method_name != "__findattr_ex__"
        || method_descriptor != "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let recv_name = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(recv_cid).map(|c| c.name.to_string())?
    };
    if recv_name != "org/python/core/PyJavaType" {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "org/python/core/PyJavaType",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

#[inline]
pub(super) fn intercept_jython_pymodule_findattr(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if method_name != "__findattr__"
        || method_descriptor != "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let recv_name = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(recv_cid).map(|c| c.name.to_string())?
    };
    if recv_name != "org/python/core/PyModule" {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "org/python/core/PyModule",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// `URLClassLoader.findClass(String)` invoked on a SUBCLASS receiver whose CP
/// methodref names that subclass (so the static-class force-native gate, keyed
/// on the methodref class, never matches). The canonical case is Jasper's
/// `JasperLoader`, which overrides BOTH `loadClass` overloads and calls
/// `findClass(name)` directly from inside `loadClass` to load the
/// runtime-compiled `org.apache.jsp.*_jsp` servlet from its scratch-dir URL.
/// Because the override runs, CratonVM's `cl_load_class` native (which would
/// resolve the class from the global classpath) is bypassed, and dispatch lands
/// on the real `URLClassLoader.findClass` bytecode — whose shimmed
/// `ucp.getResource` returns null → `ClassNotFoundException`, 500-ing every
/// compiled JSP/tag (TestPageContext, TestScopedAttributeELResolver, …).
///
/// Resolve `findClass` from the actual receiver class; only force the native
/// when it lands on `java/net/URLClassLoader` itself — a subclass that declares
/// its OWN `findClass` keeps its bytecode. `ucl_find_class` delegates to the
/// base classpath, where the loader's `<init>` already registered its URLs,
/// matching HotSpot. This handles the FIRST (uncached) dispatch; the cache
/// populator (`populate_virtual_invoke_cache`) independently force-caches the
/// native via the `declaring_name`-keyed `force_native_over_real_jdk_bytecode`
/// entry, so repeat dispatches stay native too.
#[inline]
pub(super) fn intercept_urlclassloader_subclass_native_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special
        || !matches!(
            (method_name, method_descriptor),
            ("findClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                | ("addURL", "(Ljava/net/URL;)V")
        )
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let declaring_name = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let (_m, declaring_id) = crate::classloading::find_method_recursive(
            recv_cid,
            method_name,
            method_descriptor,
            store,
        )?;
        store.get(declaring_id).map(|c| c.name.to_string())?
    };
    if declaring_name != "java/net/URLClassLoader" {
        return None;
    }
    // A JVMTI agent that redefined URLClassLoader makes its woven bytecode
    // authoritative — cede to it.
    if native_shadow_suppressed_by_redefine(shared, "java/net/URLClassLoader") {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "java/net/URLClassLoader",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// A class may invoke an inherited ClassLoader resource method through a
/// constant-pool reference to its concrete subclass.  The regular force-native
/// gate is keyed by that symbolic class, so it misses the native registered on
/// ClassLoader and the real JDK body silently accepts null names. Resolve the
/// actual declaration and dispatch the shared ClassLoader native instead.
#[inline]
pub(super) fn intercept_classloader_subclass_resource_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special
        || !matches!(
            (method_name, method_descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let declaring_name = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let (_m, declaring_id) = crate::classloading::find_method_recursive(
            recv_cid,
            method_name,
            method_descriptor,
            store,
        )?;
        store.get(declaring_id).map(|c| c.name.to_string())?
    };
    if declaring_name != "java/lang/ClassLoader"
        || native_shadow_suppressed_by_redefine(shared, "java/lang/ClassLoader")
    {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "java/lang/ClassLoader",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// Surefire's fork calls `ClassLoader.setDefaultAssertionStatus` before the JDK
/// static `assertionLock` is assigned; the real bytecode does
/// `synchronized (assertionLock)` and NPEs. Monomorphic inline caches and the
/// vtable fast path can push that bytecode without visiting `execute_invoke`, so
/// any site about to run this body must consult the Rust no-op first.
#[inline]
pub(super) fn intercept_classloader_set_default_assertion_status(
    shared: &SharedVm,
    thread: &mut JvmThread,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if method_name != "setDefaultAssertionStatus" || method_descriptor != "(Z)V" {
        return None;
    }
    // Fully-constant triple — memoized (native-dispatch-memoization §3, B2).
    // ONE STATIC, ONE TRIPLE: reached from exactly this one call, with three
    // literals. See `NCS_CLASS_GET_CLASSLOADER` for why sharing a cell across
    // triples silently mis-answers.
    static NCS_CL_SET_DEFAULT_ASSERTION_STATUS: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    let cb = NCS_CL_SET_DEFAULT_ASSERTION_STATUS.callback(
        &shared.natives.native_methods,
        "java/lang/ClassLoader",
        "setDefaultAssertionStatus",
        "(Z)V",
    )?;
    Some(crate::vm::safe_native_call(shared, thread, cb, args).map(|_| CachedCallResult::Handled))
}

/// Surefire `LazyLauncher` implements `Launcher`. Some dispatch paths key the
/// lookup by the constant-pool interface (`org/junit/platform/launcher/Launcher`)
/// while the Rust override is registered on the concrete class. When the heap
/// receiver is actually `LazyLauncher`, return that native so we never execute
/// the JDK `discover` body (null delegate → `Cannot invoke discover on null`).
#[inline]
pub(super) fn surefire_lazy_launcher_discover_native(
    shared: &SharedVm,
    method_name: &str,
    descriptor: &str,
    recv_obj: ObjectRef,
) -> Option<cratonvm_native_api::NativeCallback> {
    const DESC_DISCOVER: &str =
        "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;";
    const LAZY: &str = "org/apache/maven/surefire/junitplatform/LazyLauncher";
    if method_name != "discover" || descriptor != DESC_DISCOVER {
        return None;
    }
    // Fully-constant triple (`LAZY` / `DESC_DISCOVER` are the `const`s above),
    // memoized per native-dispatch-memoization §3 Step 1, B3.
    //
    // ONE STATIC, ONE TRIPLE. A `NativeCallSite` memo is keyed on the registry
    // generation alone — the triple is deliberately not re-checked on a warm
    // hit, since re-hashing it is the exact cost the cell exists to remove.
    // So a cell that ever sees a second triple can redeem the first triple's
    // memoized negative for the second and silently answer `None` for a
    // native that is in fact registered. This cell is reached from exactly
    // this one call, with these constants. The identical triple in
    // `native_override_for_cached_reflect_invoke` gets its *own* cell rather
    // than sharing this one.
    static NCS_LAZY_LAUNCHER_DISCOVER: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    let cb = NCS_LAZY_LAUNCHER_DISCOVER.callback(
        &shared.natives.native_methods,
        LAZY,
        "discover",
        DESC_DISCOVER,
    )?;
    let cid = shared.mem.heap.class_id_of(recv_obj);
    // read_recursive() instead of read() -- this native-override probe is
    // reached from execute_invokevirtual_vtable_fast while it already holds
    // class_manager.read() across the WP0.1 native-override check (see the
    // "ALREADY-HELD cm guard" note above that call site). A plain nested
    // read() panics the lock-order tracker (debug builds) or can deadlock
    // under parking_lot once a writer is queued (release builds) -- same
    // fix as resolve_method_ref.
    let cm = shared.classes.class_manager.read_recursive();
    let ok = cm
        .get_class(cid)
        .map(|c| c.name.as_ref() == LAZY)
        .unwrap_or(false);
    drop(cm);
    if !ok {
        return None;
    }
    Some(cb)
}

/// Will a native registered for this triple ACTUALLY run?
///
/// `native_methods.find(..).is_some()` is used all over the VM as a stand-in for
/// "this call is native-shadowed", and for most triples the two are the same
/// thing. They are NEVER the same thing for a `SyntheticStub` on a
/// [`real_protected_stub_class`] whose real body is loaded: that native loses
/// the arbitration at every dispatch site
/// ([`synthetic_stub_should_yield_to_real_bytecode`]), so the bytecode is what
/// executes and the "shadow" does not exist.
///
/// # What the difference costs
///
/// Each consumer of the raw probe pays a different penalty for the wrong
/// answer, and they compound:
///
/// * the JIT's four compile gates refuse to compile the method at all, so it
///   never reaches a tier and the megamorphic inline cache has nothing to
///   publish (`hit_entry=0`, `pub_published=0` for 506 000 consecutive calls);
/// * `jit_invoke_targets_native_shadow` then seals every CALLER of it out of
///   the JIT for the same reason;
/// * with no compiled callee to enter, every call from compiled code takes the
///   `invoke_or_native` -> `invoke_on_class_shared_inner` tail, which resolves
///   the callee from a class NAME on every single call.
///
/// MEASURED 2026-08-22 on `perf/webclient-reactive-20260821`, 200k-iteration
/// loops, HotSpot 25 control in brackets:
///
/// | call | CratonVM | HotSpot |
/// |---|---:|---:|
/// | `Instant.getNano()` - native registered, always yielded | 1627 ns | 2.3 ns |
/// | `Instant.compareTo()` - same class, no native | 39 ns | 2.9 ns |
/// | `ReentrantLock.lock()` + `unlock()` | 1760 ns | 11.9 ns |
/// | `AtomicBoolean.compareAndSet()` | 4325 ns | 5.5 ns |
/// | `LinkedBlockingDeque.peek()` | 1820 ns | 11.3 ns |
/// | `StringJoiner.length()` | 2924 ns | 5.2 ns |
/// | `Duration.getSeconds()` - control, no native | 24 ns | 2.3 ns |
///
/// Two methods of the SAME class, 40x apart, is the whole defect: `getNano` has
/// a registered native and `compareTo` does not. `java.time.Instant.now()`
/// alone runs 1 240 308 times in one `WebClientIntegrationTests` run - 475x the
/// next hottest method in that run.
///
/// # Why relaxing the gates is sound
///
/// The gates exist because "a compiled direct call bypasses the interpreter's
/// native-vs-bytecode decision". That is exactly the premise this predicate
/// checks: when the arbitration says the bytecode wins, compiling the bytecode
/// IS the interpreter's decision, not a bypass of it.
///
/// The relaxation is also one-way-safe. Every term
/// [`synthetic_stub_yields_with_cm`] reads is monotone in the direction that
/// matters - a class becomes loaded, a `Code` attribute becomes decoded - so a
/// `yield = true` verdict cannot revert while the process runs. The one thing
/// that could revert it, a JVMTI redefine, already quiesces and invalidates
/// compiled code through `any_class_redefined` /
/// `JitCache::invalidate_matching`.
///
/// Returning `true` reproduces the previous behaviour exactly for every triple
/// outside the twelve-class allow-list, and the two cheap terms in
/// [`synthetic_stub_kind_should_yield_to_real_bytecode`] short-circuit before
/// anything touches the class manager, so that is also the cost.
pub(crate) fn registered_native_will_run(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    shared
        .natives
        .native_methods
        .find(class_name, method_name, descriptor)
        .is_some()
        && !synthetic_stub_should_yield_to_real_bytecode(
            shared,
            class_name,
            method_name,
            descriptor,
        )
}

/// Synthetic stubs are fallback implementations for fake or incomplete JDK
/// classes. When the real class bytecode is loaded and explicitly protected,
/// dispatch must prefer that bytecode over the approximate stub.
pub(crate) fn synthetic_stub_should_yield_to_real_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let kind = shared
        .natives
        .native_methods
        .kind_of(class_name, method_name, descriptor);
    synthetic_stub_kind_should_yield_to_real_bytecode(
        shared,
        class_name,
        method_name,
        descriptor,
        kind,
    )
}

/// Variant for callers that already resolved and cached the native category in
/// method metadata. Keeping the selection predicate separate prevents a second
/// full triple hash on the first invocation of every constant-pool reference.
pub(super) fn synthetic_stub_kind_should_yield_to_real_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: Option<cratonvm_native_api::NativeKind>,
) -> bool {
    if kind != Some(cratonvm_native_api::NativeKind::SyntheticStub) {
        return false;
    }

    if !real_protected_stub_class(class_name) {
        return false;
    }

    let cm = shared.classes.class_manager.read();
    synthetic_stub_yields_with_cm(&cm, class_name, method_name, descriptor)
}

/// The class-manager half of [`synthetic_stub_kind_should_yield_to_real_bytecode`],
/// for callers that are **already holding** the read guard.
///
/// `populate_virtual_invoke_cache` is one: it takes `class_manager.read()` to
/// resolve the receiver's name and must ask this question before it publishes a
/// `VirtualNative` target. Re-acquiring the lock there would be a nested
/// `read()` — a lock-order panic in debug builds and a possible deadlock in
/// release once a writer is queued (the same trap
/// `threadpool_executor_has_real_workers` documented before it was deleted).
///
/// Split out rather than copied: this predicate has five terms and five
/// different remedies, and the whole point of the 2026-08-04 centralisation is
/// that no dispatch path gets to answer it differently from another.
///
/// The `kind`/allow-list gate is the CALLER's, deliberately: both terms are
/// string/enum work that must short-circuit before anyone touches the class
/// manager at all.
pub(super) fn synthetic_stub_yields_with_cm(
    cm: &crate::classloading::ClassManager,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let (verdict, why) = match cm.get_loaded_class_id(class_name) {
        None => (false, "class not loaded"),
        Some(cid) => match cm.get_class(cid) {
            None => (false, "class id not in the store"),
            Some(cls) if cls.origin.is_compatibility_stub() => {
                (false, "loaded class is itself a synthetic stub")
            }
            Some(_) => match crate::classloading::find_method_recursive(
                cid,
                method_name,
                descriptor,
                &cm.class_store,
            ) {
                None => (false, "method not found on the real class or its supers"),
                Some((m, _)) if m.is_native() => (false, "real method is ACC_NATIVE"),
                Some((m, _)) if m.is_abstract() => (false, "real method is ACC_ABSTRACT"),
                Some((m, _)) if m.code().is_none() => (false, "Code attribute NOT YET DECODED"),
                Some(_) => (true, "real bytecode wins"),
            },
        },
    };
    if dbg_stub_yield() {
        eprintln!("[STUB-YIELD] {class_name}.{method_name}{descriptor} yield={verdict} — {why}");
    }
    verdict
}

/// `CRATONVM_DBG_STUB_YIELD` — trace every allow-listed `SyntheticStub`
/// arbitration and the term that decided it.
///
/// A refusal here is silent (the stub simply runs), so "the allow-list says
/// protect this class, and the census says its stub ran anyway" had no way to
/// name which of the five terms disagreed — and those five have five different
/// fixes.
pub(crate) fn dbg_stub_yield() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STUB_YIELD").is_some())
}

/// JDK-ONLY-WAVE2: real-protected-stub class allow-list — **one predicate, both
/// dispatch paths** as of 2026-08-04.
///
/// It used to be two independently-maintained copies — this one and an inline
/// `matches!` in `vm_exec::invoke_or_native` — which is how they came to
/// differ: the cold copy listed `java/util/StringJoiner` and this one
/// deliberately omitted it, so a `SyntheticStub` native's yield-to-real-bytecode
/// verdict depended on how many times its call site had executed. The copies
/// were first centralised into one list plus one stated exception
/// (`real_protected_stub_class_cold`), and the exception was retired once the
/// defect that forced it was measured not to reproduce — see
/// [`real_protected_stub_class_common`] for that measurement.
///
/// What must ultimately replace this list: `NativeKind` alone. Under
/// `--jdk-only` a `SyntheticStub` never dispatches, so no class needs
/// protecting from one and the entire list becomes dead.
#[track_caller]
pub(crate) fn real_protected_stub_class(class_name: &str) -> bool {
    let v = crate::runtime::env_cache::real_bytecode_selector().prefers_real(class_name)
        || real_protected_stub_class_common(class_name);
    // DIAGNOSTIC (diag/which-door-arbitrates-20260824).
    //
    // Two attempts at making `java/util/ArrayList.get` yield to real bytecode
    // both ended with `invocations` unchanged at 127, in the JIT arm and under
    // `--nojit` alike, with every precondition satisfied. That is only possible
    // if the arbitration is never REACHED for this triple, and there are six
    // call sites that could reach it -- two on the static path, two in
    // `dispatch_virtual`, one inside the yield predicate itself, and one in
    // `vm_exec`.
    //
    // `#[track_caller]` rather than six hand-placed trace lines: one edit, and
    // the report cannot drift out of sync with the call sites it describes. A
    // door that never appears in this output is a door that never asks.
    if dbg_stub_door() {
        let l = std::panic::Location::caller();
        stub_door_note(l.file(), l.line(), class_name, v);
    }
    v
}

/// `CRATONVM_DBG_STUB_DOOR` — tally every `real_protected_stub_class` question
/// by CALL SITE and class, dumped at exit.
///
/// A tally, not a print-per-call: this predicate is on the dispatch path and
/// `ArrayList.get` alone asks it thousands of times, so a line per call would
/// change the thing it measures and bury the one row that matters.
pub(crate) fn dbg_stub_door() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STUB_DOOR").is_some())
}

type StubDoorKey = (&'static str, u32, String, bool);
static STUB_DOOR_TALLY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<StubDoorKey, u64>>,
> = std::sync::OnceLock::new();

fn stub_door_note(file: &'static str, line: u32, class_name: &str, verdict: bool) {
    let m =
        STUB_DOOR_TALLY.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
    if let Ok(mut g) = m.lock() {
        *g.entry((file, line, class_name.to_string(), verdict))
            .or_insert(0) += 1;
    }
}

/// `CRATONVM_DBG_NATIVE_ENTRY` — tally native-funnel entries by CALL SITE.
///
/// Companion to [`dbg_stub_door`]: that one says which doors ASK the
/// `SyntheticStub` arbitration, this one says which code path actually INVOKES
/// a native. A triple entered millions of times that appears at no door at all
/// is a dispatch path with no arbitration in it — which is the shape both were
/// built to hunt.
///
/// It earned its keep immediately. Instrumenting only `safe_native_call` showed
/// ~10 000 entries where the probe makes 600 000 native calls; the hot paths
/// use `safe_native_call_prevalidated_objects`, and once that was counted too
/// the answer was one line: `vm/src/jit/helpers.rs` x598 302, i.e. the JIT's
/// site-cached native dispatch serves essentially all of it.
pub(crate) fn dbg_native_entry() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NATIVE_ENTRY").is_some())
}

static NATIVE_ENTRY_TALLY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<(&'static str, u32), u64>>,
> = std::sync::OnceLock::new();

pub fn native_entry_note(file: &'static str, line: u32) {
    let m =
        NATIVE_ENTRY_TALLY.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
    if let Ok(mut g) = m.lock() {
        *g.entry((file, line)).or_insert(0) += 1;
    }
}

/// Print the [`dbg_native_entry`] tally, busiest call site first.
pub fn report_native_entry_tally_at_exit() {
    if !dbg_native_entry() {
        return;
    }
    let Some(m) = NATIVE_ENTRY_TALLY.get() else {
        return;
    };
    let Ok(g) = m.lock() else { return };
    let mut v: Vec<_> = g.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("[NATIVE-ENTRY] native-funnel entries, by call site:");
    for ((file, line), n) in v.into_iter().take(12) {
        eprintln!("[NATIVE-ENTRY]   {file}:{line}  x{n}");
    }
}

/// Print the [`dbg_stub_door`] tally. Called from the same exit path as the
/// other census dumps.
pub fn report_stub_door_tally_at_exit() {
    if !dbg_stub_door() {
        return;
    }
    let Some(m) = STUB_DOOR_TALLY.get() else {
        eprintln!("[STUB-DOOR] no call recorded — the predicate was never asked at all");
        return;
    };
    let Ok(g) = m.lock() else { return };
    if g.is_empty() {
        eprintln!("[STUB-DOOR] no call recorded — the predicate was never asked at all");
        return;
    }
    eprintln!("[STUB-DOOR] real_protected_stub_class questions, by call site:");
    for ((file, line, class, verdict), n) in g.iter() {
        eprintln!("[STUB-DOOR]   {file}:{line}  {class}  -> {verdict}  x{n}");
    }
}

/// The classes **both** dispatch paths yield to real bytecode.
///
/// Kept as a `matches!` over string literals rather than a slice scan: this is
/// on the native-dispatch path, and `matches!` compiles to a length-bucketed
/// comparison chain rather than one `str` equality call per entry.
#[inline]
fn real_protected_stub_class_common(class_name: &str) -> bool {
    // `java/util/ArrayList` is the ONE conditional member of this list, and it
    // sits here rather than in the `matches!` below because of it. The reason
    // the class is allow-listed at all is in the comment down there, with its
    // measurements; the reason it needs a condition is this:
    //
    // `force_native_over_real_jdk_bytecode`'s ArrayList entry says a
    // `Map.values()` view IS a plain `java/util/ArrayList` that stashes its
    // source map in the last capacity slot of its element array and must
    // re-sync against it on read. That stopped being true on 2026-08-13 —
    // views are minted under `MAP_VIEW_CARRIERS`, which are NOT on this list
    // and ARE force-listed, so they keep their natives — and that is what makes
    // allow-listing `java/util/ArrayList` safe. But `alloc_view_carrier` still
    // has a last-resort arm that degrades to `java/util/ArrayList` when the
    // carrier class cannot be had at all. A view minted THERE, served by real
    // `ArrayList` bytecode, returns whatever its source held at creation: the
    // `Schema.getAllSequences()` shape (H2 `TestAlter`).
    //
    // So that arm publishes, and this asks. Default is "no such view exists",
    // i.e. the yield is ON — the opposite default was built first and measured
    // INERT, because a program that never calls `values()` never publishes
    // anything and the answer is memoized per call site as each site warms.
    // `cratonvm_types::arraylist_view` states the residual this leaves, and why
    // it is not reachable on any image a JDK ships.
    if class_name == "java/util/ArrayList" {
        return !cratonvm_types::arraylist_view::arraylist_classed_view_possible();
    }
    // `java/util/ArrayList$Itr`, for `hasNext`/`next`, retagged `SyntheticStub`
    // at their registration. 6.2x on `probes/KeySetBench iterList`, and it also
    // FIXES `cmeClear` -- the native did not throw `ConcurrentModificationException`
    // when a list was cleared mid-iteration.
    //
    // BOTH HALVES ARE REQUIRED AND NEITHER WORKS ALONE. Two earlier arms read as
    // failures for two different reasons, and both were the measurement rather
    // than the fix: allow-list alone does nothing (term 1 is
    // `kind != SyntheticStub -> refuse`), and retag alone is 1.56x SLOWER
    // (`resolve_native_site` refuses to cache ANY `SyntheticStub`, so the site
    // falls out of the JIT's native cache onto the generic path -- still running
    // the native, by the expensive route).
    //
    // AND IT IS SOUND ONLY BECAUSE OF THE MINT SPLIT.
    // `itr_backing_has_real_mod_count` gives any backing WITHOUT a real
    // `modCount` a different class (`cratonvm/internal/ArrayListViewItr`), so
    // this name is a guarantee. Before that split, `for (v : map.values())`
    // threw a spurious CME: HotSpot's `checkForComodification` read a view
    // carrier's first declared REFERENCE field as an int.
    //
    // Do NOT re-derive that guarantee from a class census. `probes/ItrClassProbe`
    // said `IdentityHashMap`'s values view minted its own iterator, matching
    // HotSpot -- and it still arrived here and threw. The class a receiver mints
    // in isolation is not the class it reaches the `al_itr_*` natives with.
    if class_name == "java/util/ArrayList$Itr" {
        return true;
    }
    matches!(
        class_name,
        "java/util/concurrent/locks/ReentrantLock"
            | "java/util/concurrent/LinkedBlockingDeque"
            | "java/util/concurrent/atomic/AtomicBoolean"
            | "java/util/EnumSet"
            // The permission family. `register_essential_natives` replaces
            // their `(String)` / `(String,String)` constructors with a closure
            // that writes `name` and returns — correct when this VM FABRICATED
            // these classes (`b448f2039`), and a silent mutilation of the real
            // ones now that those are loaded: `BasicPermission.init(name)` and
            // `PropertyPermission.init(getMask(actions))` never ran, leaving
            // `path` null and `mask` 0, so `implies()` NPEd, an invalid actions
            // string was accepted where the JDK throws, and a serialization
            // round trip died with "invalid actions mask".
            //
            // Correctness, not throughput — and the whole family, because the
            // closure is registered over all five in one loop and every one of
            // them inherits a real `init` from `BasicPermission`.
            | "java/security/Permission"
            | "java/security/BasicPermission"
            | "java/lang/RuntimePermission"
            | "java/util/PropertyPermission"
            | "java/util/logging/LoggingPermission"
            // `java/util/Objects` is registered TWICE, and only the
            // real-JDK-path registration is a `SyntheticStub`:
            // `register_synthetic_overrides` installs it `Intrinsic` (on a
            // synthetic image those bodies ARE the implementation and this
            // entry cannot reach them, because the kind gate runs first),
            // while `register_annotation_overrides` -- the real-JDK essentials
            // path -- installs it `SyntheticStub` as a partial-stub-boot
            // fallback. This entry arms the yield for that second one.
            //
            // Unlike its neighbours the motive is throughput, not correctness:
            // the stub answers the same as the real bytecode, but winning
            // meant `Objects.equals` / `hashCode` / `requireNonNull` /
            // `isNull` could never be JIT-compiled, because a static's
            // native-vs-bytecode arbitration is `NativeKind` alone.
            // `requireNonNull` is among the most-called methods in the JDK and
            // in Spring, so it was a VM-wide tax. Measured 10M calls,
            // nativized against a byte-identical local static: equals
            // 203.5 -> 47.2 ns, hashCode 174.1 -> 47.8, requireNonNull
            // 124.3 -> 48.6, isNull 91.8 -> 24.6.
            | "java/util/Objects"
            // `java/util/ArrayList`, for `get`/`size`/`isEmpty` -- whose natives
            // are already `SyntheticStub` (the census says so; the ambient
            // `set_category(Bridge)` around their registration is NOT what
            // lands, because `register` adjudicates the kind itself). Like the
            // `Objects` entry above, the motive is throughput: the native cost
            // 11x its own JDK bytecode, because pinning the native also pins
            // the method out of the JIT.
            //
            // MEASURED, `probes/KeySetBench idxList` (1000 `list.get(i)` calls
            // per row), us/call, three interleaved rounds, against the same
            // binary with this one entry removed:
            //
            //     idxList        752 -> 68     11.1x
            //     toArrHoisted   110 -> 96      1.15x   (a bonus; Spring's door)
            //     rawArr          19 -> 18      flat    (control: no collection)
            //     iterList       987 -> 971     flat    (the iterator does not
            //                                            go through `get`)
            //
            // Two earlier attempts concluded this "did not engage" and were
            // reverted. Both were wrong, and both were wrong for the same
            // reason: they were scored on the native census's `invocations`,
            // which SATURATES -- it reads the same 127 at 5 000 and 50 000
            // iterations, and prints `invocations_complete: false` beside the
            // number to say so. The arbitration was always reached (a
            // `#[track_caller]` tally on this predicate shows this class asked
            // at all three virtual doors) and the yield always worked; only the
            // instrument was blind. Score this class by TIME.
            //
            // The entry itself is NOT here: it is the guarded early return at
            // the top of this function, because it is the one member of this
            // list that is conditional. See there, and see
            // `cratonvm_types::arraylist_view`.
            // JDK-ONLY-WAVE2, 2026-08-06. `native_es_execute` (retagged
            // `SyntheticStub` in `native-builtins/src/util_concurrent_ext.rs`)
            // is the compatibility stand-in for CratonVM's synthetic 2-field
            // `Executors.new*ThreadPool()` receiver. Every factory shortcut now
            // drives the real `ThreadPoolExecutor.<init>`
            // (`initialize_real_thread_pool_executor`), so on an image where
            // the real class bytes are loaded there is no synthetic receiver
            // left and the real `execute()` body is right for ALL of them.
            //
            // This entry is what replaced eight hand-written receiver-shape
            // probes ("does the receiver's `workers` field hold an object?")
            // spread across four files in `vm`. It answers the same question
            // class-scoped instead of per-instance, which is only sound
            // *because* the per-instance distinction was eliminated first —
            // reinstating a synthetic-layout `ThreadPoolExecutor` producer
            // without also reinstating those probes would send it to real
            // bytecode that dereferences a null `ctl`/`mainLock`.
            //
            // On a synthetic-JDK image the class IS a compatibility stub, the
            // predicate below finds no real `execute()` body, and the native
            // still runs — which is the only mode that still needs it.
            | "java/util/concurrent/ThreadPoolExecutor"
            // The fallback bridge is needed only if bootstrap had to
            // synthesize Instant.  With a loaded real JDK Instant, every
            // factory must run its real bytecode so the result has the
            // real field layout and ISO-8601 `toString()` semantics.
            | "java/time/Instant"
            // Spring Boot's loader decodes central-directory DOS times via
            // ZonedDateTime.of(...). The synthetic bridge stores its
            // fields in a compact layout that is incompatible with the
            // loaded JDK class, turning historical ZIP timestamps into
            // the current clock value when converted to an Instant.
            | "java/time/ZonedDateTime"
            | "java/io/FileInputStream"
            | "java/lang/ref/Cleaner"
            | "java/lang/ref/Cleaner$Cleanable"
            | "java/lang/management/ManagementFactory"
            // Protected on BOTH paths since 2026-08-04. It was cold-path-only
            // from 2026-07-10, because yielding this class's SyntheticStub
            // natives to real bytecode on the warm path once tripped a
            // deterministic heap-reference-integrity defect: the
            // `gen_heap::read_slot` "corrupt Value cell" / HIB-CV-32 guard
            // fired reading `StringJoiner`'s own `size` / `elts` back after a
            // `putfield`, from the SECOND `add()` onward. Re-measured
            // 2026-08-04 with the merge applied — 40k `add()` calls under
            // `-Xmx64m` with per-iteration allocation churn, seven intermediate
            // consistency checks, against a HotSpot control — and it did not
            // reproduce on either `--jdk-only` or `--real-jdk`.
            //
            // Dropping it from the cold path instead is the other half of the
            // trap and is NOT an option: the synthetic `add()` writes a 5-field
            // fake layout (`delim/prefix/suffix/elements-ArrayList/emptyValue`)
            // over the real 7-field class
            // (`prefix/delimiter/suffix/elts[]/size/len/emptyValue`), reads
            // slot 3 — real `elts`, null — and no-ops, so `size` never moves
            // and `toString()` renders just prefix+suffix. A silently empty
            // join, not a crash. See
            // `stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md`.
            | "java/util/StringJoiner"
    )
}

/// Every class the allow-list protects, as a floor corpus for
/// `every_allowlisted_class_is_protected`.
///
/// Not a dispatch input — the predicate above stays a `matches!`. It exists so
/// that deleting a class from the allow-list fails a test instead of silently
/// handing that class's `SyntheticStub` natives back to a synthetic layout.
#[cfg(test)]
pub(crate) const REAL_PROTECTED_STUB_CORPUS: &[&str] = &[
    "java/util/concurrent/locks/ReentrantLock",
    "java/util/concurrent/LinkedBlockingDeque",
    "java/util/concurrent/atomic/AtomicBoolean",
    "java/util/EnumSet",
    "java/util/concurrent/ThreadPoolExecutor",
    "java/time/Instant",
    "java/time/ZonedDateTime",
    "java/io/FileInputStream",
    "java/lang/ref/Cleaner",
    "java/lang/ref/Cleaner$Cleanable",
    "java/lang/management/ManagementFactory",
    "java/util/StringJoiner",
];

#[cfg(test)]
mod real_protected_stub_tests {
    use super::*;

    /// Every class the allow-list is supposed to protect is still protected.
    ///
    /// This replaces `real_protected_stub_paths_diverge_on_exactly_stringjoiner`
    /// (2026-07-10 → 2026-08-04), which froze the one-class divergence between
    /// the warm and cold dispatch paths. There is now a single predicate, so
    /// there is nothing left to compare the paths against — an "the paths
    /// agree" assertion over one function would be a guard that cannot fail.
    /// What can still regress is a class silently leaving the list, so that is
    /// what is asserted, with a floor on the corpus size so that emptying the
    /// corpus does not make the test vacuous either.
    ///
    /// It does not cover the `CRATONVM_REAL` env selection, which the predicate
    /// ORs in from `real_bytecode_selector()`; that widens the list, never
    /// narrows it.
    #[test]
    fn every_allowlisted_class_is_protected() {
        assert!(
            REAL_PROTECTED_STUB_CORPUS.len() >= 12,
            "the corpus shrank to {} entries; a class was removed from the \
             real-protected-stub allow-list. That hands its `SyntheticStub` natives \
             back to a synthetic field layout over the real JDK class — for \
             `StringJoiner` that was a silently empty join, not a crash. If the \
             removal is intended, lower this floor deliberately and say why.",
            REAL_PROTECTED_STUB_CORPUS.len()
        );
        for class in REAL_PROTECTED_STUB_CORPUS {
            assert!(
                real_protected_stub_class(class),
                "{class} is in REAL_PROTECTED_STUB_CORPUS but the allow-list no longer \
                 protects it; restore it to `real_protected_stub_class_common` or \
                 remove it from the corpus and lower the floor above"
            );
        }
    }
}

#[cfg(test)]
mod threadpool_receiver_shape_tests {
    /// The `vm` crate's source root, for the scan below.
    fn vm_src(rel: &str) -> String {
        // `rel` is repo-relative (`vm/src/...`) so the paths read the way an
        // `rg` invocation would; strip the crate prefix to get a path under
        // this crate's manifest dir.
        let under_crate = rel
            .strip_prefix("vm/")
            .expect("scan paths are vm-crate paths");
        let path = format!("{}/{under_crate}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
    }

    /// The four files that carried the eight `ThreadPoolExecutor.execute`
    /// receiver-shape probes, retired 2026-08-06.
    const FORMER_SITE_FILES: &[&str] = &[
        "vm/src/vm/vm_exec.rs",
        "vm/src/runtime/interpreter/invoke.rs",
        "vm/src/runtime/interpreter/native_override.rs",
        "vm/src/runtime/interpreter/dispatch_virtual.rs",
    ];

    /// **Zero** dispatch sites probe the receiver's shape, and none of them
    /// re-inlines the `workers` lookup by hand either.
    ///
    /// This replaces `exactly_eight_dispatch_sites_probe_the_threadpool_receiver_shape`
    /// and `no_hand_inlined_workers_probe_outside_the_helper` (2026-08-04 →
    /// 2026-08-06), which pinned the count at eight so that a *partial* sweep
    /// would fail. The sweep happened; what can regress now is a copy growing
    /// back, one site at a time, the way the original eight did — each for a
    /// real observed failure, each locally reasonable.
    ///
    /// If you are here because this test failed: a receiver-shape probe is the
    /// wrong fix. The class-scoped arbitration
    /// (`real_protected_stub_class_common` + `NativeKind::SyntheticStub` on
    /// `native_es_execute`) is only correct while every
    /// `ThreadPoolExecutor`-tagged object on a real-JDK image is genuinely
    /// real. If something reintroduced a synthetic-layout producer, fix THAT —
    /// make it drive `initialize_real_thread_pool_executor` — rather than
    /// teaching one dispatch path to tell the two apart again.
    #[test]
    fn every_threadpool_receiver_shape_site_is_gone() {
        // Assembled at runtime so this scanner's own source does not contain
        // the strings it looks for.
        let probe_call = format!("{}(", "threadpool_executor_has_real_workers");
        let hand_inlined = format!(
            r#"{}(recv_class_id, "workers""#,
            "resolve_field_index_in_hierarchy"
        );

        for file in FORMER_SITE_FILES {
            let src = vm_src(file);
            assert!(
                !src.contains(probe_call.as_str()),
                "{file} calls the deleted ThreadPoolExecutor receiver-shape probe again. \
                 All eight sites and the probe itself were removed on 2026-08-06 together \
                 with the receiver-blind ninth arm in `force_native_over_real_jdk_bytecode`; \
                 re-adding one alone recreates the cold-path/warm-path split the census \
                 constant existed to prevent."
            );
            assert!(
                !src.contains(hand_inlined.as_str()),
                "{file} hand-inlines the ThreadPoolExecutor `workers` probe. That is how \
                 the predicate came to have three implementations, two of which took a \
                 plain `read()` where a nested `read_recursive()` is required."
            );
        }
    }

    /// The class-scoped replacement is actually in place.
    ///
    /// Two independent facts, and the pair is the whole fix: without the
    /// allow-list entry the retagged native would win over real bytecode for
    /// every receiver (the recursion the eight probes prevented); without the
    /// retag the allow-list entry is inert, because the arbitration only fires
    /// for `NativeKind::SyntheticStub`.
    ///
    /// The `NativeKind` half is pinned in
    /// `native-builtins/tests/stub_ratchet.rs`, which builds the real boot
    /// registry; this half pins the allow-list, which is `vm`'s.
    #[test]
    fn threadpool_executor_is_real_protected() {
        assert!(
            super::real_protected_stub_class("java/util/concurrent/ThreadPoolExecutor"),
            "`java/util/concurrent/ThreadPoolExecutor` left the real-protected-stub \
             allow-list. `native_es_execute` is tagged `SyntheticStub`, so without this \
             entry it wins over the real `execute()` body for every receiver — including \
             the async worker pool a native calls `.execute()` on, which recurses into \
             the same native and aborts the process rather than throwing."
        );
    }

    /// The scan corpus above is non-empty and every path in it is real.
    ///
    /// `every_threadpool_receiver_shape_site_is_gone` is a loop over
    /// `FORMER_SITE_FILES` asserting an absence. **A loop over nothing asserts
    /// nothing** — rename or move one of those four files and the gate keeps
    /// passing while scanning three, or zero, and the regression it exists to
    /// catch walks straight through the file it stopped reading.
    ///
    /// That is the same failure as the count this module's predecessor froze at
    /// eight silently becoming zero, which is why the count gate was replaced
    /// rather than emptied. An absence-gate needs its corpus pinned for exactly
    /// the reason a presence-gate needs its number pinned.
    ///
    /// `vm_src` already panics on an unreadable path, so this adds the two
    /// things it cannot check: that the list is not empty, and that a path
    /// resolving to something implausibly small (a stub left behind by a move)
    /// is not silently accepted as "no matches found".
    #[test]
    fn the_receiver_shape_scan_set_is_real() {
        assert!(
            !FORMER_SITE_FILES.is_empty(),
            "FORMER_SITE_FILES is empty, so `every_threadpool_receiver_shape_site_is_gone` \
             scans nothing and cannot fail"
        );
        for file in FORMER_SITE_FILES {
            let src = vm_src(file);
            assert!(
                src.len() > 10_000,
                "{file} is {} bytes — far smaller than any of the four dispatch files that \
                 carried the receiver-shape probes. The scan is reading the wrong path, and \
                 the gate above is passing vacuously over it.",
                src.len()
            );
        }
    }
}

#[cfg(test)]
mod forced_native_string_tests {
    use super::*;

    /// Every `java/lang/String` shape either half of the deleted policy named,
    /// plus the ones it deliberately left out.
    ///
    /// The old table froze *which* shapes the cold and warm halves disagreed
    /// about. There is nothing left to disagree: no dispatch path matches
    /// `java/lang/String` by name any more, so the answer for every shape is
    /// the same one, and that uniformity is what this pins.
    ///
    /// Descriptors are the real JDK 25 ones.
    const STRING_SHAPES: &[(&str, &str)] = &[
        // The five the h2-bnf block named, and the two `substring` overloads.
        ("substring", "(I)Ljava/lang/String;"),
        ("substring", "(II)Ljava/lang/String;"),
        ("charAt", "(I)C"),
        ("length", "()I"),
        ("isEmpty", "()Z"),
        ("startsWith", "(Ljava/lang/String;)Z"),
        // The SBR-02 fast-regex family and the charset-name constructors —
        // the shapes the warm whitelist admitted and the cold list never
        // mentioned.
        (
            "replaceAll",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        ),
        (
            "replaceFirst",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        ),
        ("matches", "(Ljava/lang/String;)Z"),
        ("<init>", "([BLjava/lang/String;)V"),
        ("<init>", "([BIILjava/lang/String;)V"),
        (
            "replace",
            "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;",
        ),
        ("replace", "(CC)Ljava/lang/String;"),
        // The Unicode/locale-sensitive residue the cold list forced and the
        // warm list refused — the actual divergence, now moot.
        ("trim", "()Ljava/lang/String;"),
        ("toLowerCase", "()Ljava/lang/String;"),
        ("toLowerCase", "(Ljava/util/Locale;)Ljava/lang/String;"),
        ("toUpperCase", "()Ljava/lang/String;"),
        ("compareTo", "(Ljava/lang/String;)I"),
        ("compareToIgnoreCase", "(Ljava/lang/String;)I"),
        ("equalsIgnoreCase", "(Ljava/lang/String;)Z"),
        ("split", "(Ljava/lang/String;)[Ljava/lang/String;"),
        // Plain shapes the cold list forced and the warm list never reviewed.
        ("equals", "(Ljava/lang/Object;)Z"),
        ("hashCode", "()I"),
        ("indexOf", "(Ljava/lang/String;)I"),
        ("lastIndexOf", "(Ljava/lang/String;)I"),
        ("endsWith", "(Ljava/lang/String;)Z"),
        ("toString", "()Ljava/lang/String;"),
        ("concat", "(Ljava/lang/String;)Ljava/lang/String;"),
        ("contains", "(Ljava/lang/CharSequence;)Z"),
        // On neither list, then or now.
        ("chars", "()Ljava/util/stream/IntStream;"),
        ("strip", "()Ljava/lang/String;"),
    ];

    /// No dispatch path forces a native over `java/lang/String` bytecode by
    /// name any more — for **any** shape, including the twelve the warm
    /// whitelist used to admit.
    ///
    /// This replaces `forced_native_string_policy_divergence_is_exactly_the_
    /// unicode_sensitive_names`, which froze a divergence rather than removing
    /// it. The divergence is gone because the policy is: whichever
    /// `java/lang/String` natives are *registered* in real-JDK mode win, and
    /// `NativeMethodRegistry::register`'s real-JDK drop decides which those
    /// are, once, for every path.
    ///
    /// Verified by injection: restoring any `class_name == "java/lang/String"`
    /// arm to `force_native_over_real_jdk_bytecode` fails this immediately.
    #[test]
    fn no_string_shape_is_forced_native_by_name_on_any_dispatch_path() {
        for &(name, descriptor) in STRING_SHAPES {
            assert!(
                !force_native_over_real_jdk_bytecode("java/lang/String", name, descriptor),
                "String.{name}{descriptor} is forced native by name again. The forced-native \
                 `java/lang/String` policy was removed on 2026-08-04 after being MEASURED \
                 inert — a binary with both lists deleted produced a byte-identical 392-case \
                 `String` transcript in both modes and identical invocation counts on all 38 \
                 exercised registry slots. If a `String` native must win, register it \
                 `NativeKind::Intrinsic` and let §1.4 take it on kind; do not add a name here, \
                 because a name here decides nothing (`resolve_step1_native` dispatches the \
                 triple before this function runs) while reading exactly like it does."
            );
        }
    }

    /// The predicate is not simply `false` for everything — the assertion above
    /// has to be capable of failing.
    ///
    /// A guard that cannot fail reads exactly like one that works; three were
    /// found in one evening on this feature. `java/lang/StringUTF16.getChars`
    /// is a deliberate, still-live entry in the same function, so a `true`
    /// here proves the `false`s above are decisions rather than a stub.
    #[test]
    fn the_string_guard_can_fail() {
        assert!(
            force_native_over_real_jdk_bytecode("java/lang/StringUTF16", "getChars", "([BII[CI)V"),
            "`force_native_over_real_jdk_bytecode` no longer forces the one entry that proves \
             it still forces anything, so `no_string_shape_is_forced_native_by_name_on_any_\
             dispatch_path` above is vacuous"
        );
    }

    /// The source of every dispatch path is free of `java/lang/String` name
    /// matching.
    ///
    /// The predicate test above only covers `force_native_over_real_jdk_bytecode`.
    /// The policy had three copies, and the one that cost a session to find was
    /// in a different file — so scan the other two directly.
    #[test]
    fn no_dispatch_path_mentions_java_lang_string_by_name() {
        // (file, what a match there would mean)
        let scanned: &[(&str, &str)] = &[(
            include_str!("../../vm/vm_exec.rs"),
            "vm/src/vm/vm_exec.rs's `check_override` chain",
        )];
        for (src, what) in scanned {
            // Establish the corpus is real before concluding anything from an
            // absence: a mis-typed `include_str!` path would not compile, but a
            // file that stopped containing the chain at all would make this
            // pass for the wrong reason.
            //
            // The anchor was `let check_override = method.is_abstract()` until
            // 2026-08-10, when the name half was split out as `name_override`
            // so strict mode could skip it. This assertion firing is exactly
            // how that was noticed, instead of the scan quietly reading a file
            // that no longer held the thing it was scanning for.
            assert!(
                src.contains("let name_override = class_name =="),
                "{what} no longer contains the `check_override` chain, so this scan is \
                 looking at the wrong text and its absence-of-`String` verdict means nothing"
            );
            // Match the FORCE-NATIVE shape specifically — a disjunct of the
            // `check_override` chain — rather than every mention of the class
            // name. `invoke_or_native` also carries a `java/lang/String` arm
            // that SUPPRESSES a bogus inherited `setOption` dispatch
            // (`return Ok(None)`), which is the opposite of forcing a native
            // and must not trip this. Caught by this guard on its first run,
            // which is the argument for writing it as a scan rather than as a
            // claim.
            for (i, line) in src.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                assert!(
                    !code.starts_with(r#"|| (class_name == "java/lang/String""#),
                    "{what} has a forced-native `java/lang/String` disjunct again at line \
                     {}. That arm was deleted on 2026-08-04 after being measured inert; \
                     re-adding one re-creates the cold/warm divergence whose whole cost was \
                     that a `String` method's behaviour started depending on how many times \
                     its call site had executed. If a `String` native must win, register it \
                     `NativeKind::Intrinsic`: `NativeMethodRegistry::register`'s real-JDK \
                     drop is where this policy lives now.",
                    i + 1
                );
            }
        }
    }
}

/// [`try_stackless_invoke`] step 1's primary native lookup, routed through the
/// §7 policy (`docs/feature-designs/jdk-only-mode.md`).
///
/// Replaces the bare `native_methods.find(class_name, method_name, descriptor)`
/// that sat at the head of step 1's `.or_else` chain. A lookup that throws the
/// `NativeKind` away cannot tell a reviewed `Intrinsic` from a `SyntheticStub`,
/// so it cannot enforce §1.3 or §1.4 — it was a policy bypass sitting beside
/// `resolve_dispatch` instead of routing through it.
///
/// Cost is unchanged. `resolve_id` is the *same* single 128-bit triple hash
/// `find` already paid — `resolve_id(..).and_then(callback_of)` is documented
/// to equal `find(..)`, descriptor-quirk fallback included — and the slot
/// handle it returns makes the §4 census increment one relaxed add instead of a
/// second full hash. Nothing is allocated or formatted; violation objects are
/// built only on the reject path, in `vm_exec`'s `#[cold]` constructors.
///
/// `id_out` carries the resolved handle back to the caller so the census is
/// recorded at the point of actual **dispatch**, not here at resolution: three
/// later guards (JVMTI redefine, synthetic-stub yield, the `ThreadPoolExecutor`
/// receiver check) can still discard this callback, and counting a discarded
/// resolution as an invocation would make the zero-stub acceptance criterion
/// unfalsifiable in the wrong direction.
///
/// `refusal` is an out-parameter rather than a `Result` because this runs
/// inside an `Option`-returning `.or_else` chain; the caller checks it once,
/// after the chain, and turns it into `VmError::JdkOnly`.
///
/// **`Compatible` mode is bit-for-bit today's behaviour**: `compat_native_wins`
/// is `true`, which is exactly the unconditional "a registered native wins
/// here" the `find` call encoded, and in `Compatible` mode
/// `resolve_native_dispatch_wave1` is a pure function of that boolean.
#[inline]
pub(super) fn resolve_step1_native(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    // The loader-precise dispatch class the call site already computed, when it
    // has one. Only consumed by the strict-mode `bytecode_available` probe
    // below, and only to start the hierarchy walk at the same class the invoke
    // itself will — see [`step1_dispatch_has_code`].
    dispatch_class_override: Option<crate::classloading::ClassId>,
    id_out: &mut Option<cratonvm_native_api::NativeMethodId>,
    refusal: &mut Option<cratonvm_types::error::JdkOnlyViolation>,
) -> Option<cratonvm_native_api::NativeCallback> {
    let registry = &shared.natives.native_methods;
    let id = registry.resolve_id(class_name, method_name, descriptor)?;
    let callback = registry.callback_of(id)?;
    // `kind_of_id` reports the slot's true kind. `find_with_kind` would report
    // `Bridge` on its descriptor-quirk cold path; the difference is invisible
    // in `Compatible` mode (every kind yields the same callback) and strictly
    // more accurate under `JdkOnly`.
    let kind = registry
        .kind_of_id(id)
        .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
    let policy = crate::vm::dispatch_policy(shared);
    // §7 step 3's input, resolved LAZILY and only where it is both consulted
    // and affordable.
    //
    // Step 1 runs before method resolution so the common "no native registered"
    // case never touches the class manager — `resolve_id(..)?` above has
    // already returned for every such triple, so this walk only ever runs for a
    // triple that HAS a registration. Narrowing further, `Bridge` is the only
    // kind that consults the flag (`SyntheticStub` is refused and `Intrinsic`
    // is taken regardless), and every consumer is `is_jdk_only()`-gated. So a
    // default `--real-jdk` run pays one `Copy` field read and one enum compare,
    // and nothing else.
    //
    // The 2026-08-05 attempt asked the cheaper question — "does the NAMED class
    // declare bytecode", from access flags. That is not the same question as
    // "does the method this call will actually run have a `Code` attribute" the
    // moment a hierarchy is involved. `step1_dispatch_has_code` asks the second
    // one. See its doc comment.
    //
    // ENFORCEMENT IS A SEPARATE DECISION FROM OBSERVATION, and this is where
    // the two part company:
    //
    // * The observation is unconditional. A `Bridge` in front of real bytes is
    //   §1.4's `NativeShadowsBytecode` whether or not anything is done about
    //   it, and step 1 answers first for nearly every dispatch in the VM — so
    //   with the old hard-coded `false` the census could not see the shadows
    //   that were actually dispatching, only the ones some other site caught.
    // * The enforcement is off by default, and that is measured rather than
    //   preferred: arming it takes the `--jdk-only` regression corpus from
    //   32/17 to 3/46, because the surviving bridges ARE the object model for
    //   large parts of `java.base` under strict mode. See
    //   `env_cache::jdk_only_enforce_shadow` for the numbers, and
    //   `jdk-only-step1-bytecode-available-RESOLVED-20260806.md`
    //   for all five blocker families with their symptoms.
    let strict_bridge = policy.is_jdk_only() && kind == cratonvm_native_api::NativeKind::Bridge;
    // SCOPED, not global: `jdk_only_enforce_shadow()` answers "is anything
    // armed at all", which under a prefix list is true for every class. Asking
    // it here would enforce one subsystem's dial across the whole VM — the 3/46
    // collapse the scoping exists to avoid. See `enforce_shadow_scope`.
    let enforce =
        strict_bridge && crate::runtime::env_cache::jdk_only_enforce_shadow_for(class_name);
    // When the shadow is only being observed, the walk is worth doing at most
    // once per triple — the sink dedups, so a second walk buys nothing — and
    // not at all once the sink saturates, since it can no longer learn a new
    // one. That is what makes `interpreter_shadow_unenforced` a floor rather
    // than an exact count, stated where it is read. When the shadow is being
    // ENFORCED the answer is a dispatch input and must be current, so the walk
    // runs every time.
    let ask = strict_bridge
        && (enforce
            || !crate::vm::jdk_only_shadow_already_observed(
                class_name,
                method_name,
                descriptor,
                crate::vm::JDK_ONLY_SHADOW_UNENFORCED_TAG,
            ));
    let shadows_bytecode = ask
        && step1_dispatch_has_code(
            shared,
            class_name,
            method_name,
            descriptor,
            dispatch_class_override,
        );
    if shadows_bytecode && !enforce {
        // The bridge is about to win in front of real bytes. Record it here;
        // `resolve_native_dispatch_wave1` records only on the yield path, and
        // it is not taking that path (`bytecode_available` is false below), so
        // this cannot double-count.
        crate::vm::record_native_shadow_ran_over_bytecode(class_name, method_name, descriptor);
    }
    match crate::vm::resolve_native_dispatch_wave1(
        crate::vm::DispatchDoor::Step1,
        policy,
        class_name,
        method_name,
        descriptor,
        Some((callback, kind)),
        // JDK-ONLY-WAVE2: hard-coded `true` reproduces the pre-§7 "a registered
        // native unconditionally wins here" of the `find` call this replaces.
        // The three per-site compatibility verdicts that can still veto it run
        // AFTER this chain (see the `native_cb` rebindings below); wave 2 folds
        // them in as the real `compat_native_wins`.
        true,
        shadows_bytecode && enforce,
    ) {
        Some(crate::vm::DispatchDecision::Reject(violation)) => {
            *refusal = Some(violation);
            None
        }
        Some(decision) => match decision.native_callback() {
            Some(callback) => {
                *id_out = Some(id);
                Some(callback)
            }
            // `Bytecode` is unreachable from the name-only adapter, but treat
            // it as "no native" rather than assuming.
            None => None,
        },
        // JdkOnly, §7 step 3: concrete bytecode beats this bridge.
        None => None,
    }
}

/// Does the method this invoke is about to run have a `Code` attribute?
///
/// This is §7 step 3's real input, and it is deliberately NOT "does the class
/// named at the call site declare bytecode". `java/nio/charset/CharsetDecoder`
/// declares `decodeLoop` **abstract** and every concrete decoder overrides it;
/// answering from the named class's access flags therefore says "no code" for a
/// call that will run a subclass body, and "code" for one that will not.
/// `find_method_recursive` is the same resolution the interpreter's own
/// dispatch uses — superclass chain first, preferring a non-abstract match,
/// then interface defaults, then an abstract declaration as a last resort — so
/// the answer here is the answer at the invoke.
///
/// Two deliberate conservatisms, both of which reproduce the pre-2026-08-06
/// "native wins" behaviour rather than inventing a new one:
///
/// * The class is looked up with `get_loaded_class_id`, never loaded. Step 1
///   must not be able to trigger class loading (and with it `<clinit>`) from
///   inside a dispatch decision, and a class that is not loaded has no bytecode
///   to prefer yet.
/// * An abstract or `ACC_NATIVE` resolution has no `Code`, so it is `false` and
///   the bridge keeps the call. That is the case the failed attempt got wrong
///   in the other direction.
///
/// Cost is one class-manager read lock plus one hierarchy walk, paid only under
/// `--jdk-only` and only for a triple that already has a `Bridge` registration.
fn step1_dispatch_has_code(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    dispatch_class_override: Option<crate::classloading::ClassId>,
) -> bool {
    // `reentrant: false` -- step 1 runs before method resolution and holds no
    // class-manager guard, so the plain `read()` this function has always taken
    // stays exactly what it was. See [`dispatch_has_code`] for why the other
    // callers cannot use it.
    dispatch_has_code(
        shared,
        class_name,
        method_name,
        descriptor,
        dispatch_class_override,
        false,
    )
}

/// [`step1_dispatch_has_code`]'s body, with the lock acquisition made a
/// parameter.
///
/// # Why `reentrant` is not a style choice
///
/// `class_manager` is an [`OrderedPlRwLock`](cratonvm_types::lock_order::OrderedPlRwLock),
/// and `parking_lot`'s plain `read()` is **not reentrant**: a thread that
/// already holds a read guard and takes another one deadlocks against a queued
/// writer. Under lock-order enforcement that is a panic, which is loud and
/// findable; enforcement is compiled out of a release build, where the same
/// code is a silent hang instead.
///
/// [`jdk_only_dial_yields_to_bytecode`] is called from doors that DO hold the
/// guard -- `invoke_or_native`'s superclass walk holds it across the whole walk
/// -- so it passes `true` and gets `read_recursive()`, which is documented as
/// existing for exactly this case. Re-entering a lock the thread already holds
/// adds no edge to the wait-for graph.
fn dispatch_has_code(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    dispatch_class_override: Option<crate::classloading::ClassId>,
    reentrant: bool,
) -> bool {
    let cm = if reentrant {
        shared.classes.class_manager.read_recursive()
    } else {
        shared.classes.class_manager.read()
    };
    // Same start class the call site's own superclass walk uses: the
    // loader-precise override when the caller resolved one, else the flat
    // name lookup.
    let Some(start) = dispatch_class_override.or_else(|| cm.get_loaded_class_id(class_name)) else {
        return false;
    };
    // Routed through `MemberResolver` rather than a bare
    // `find_method_recursive`, which is what `runtime::resolve::guard`'s
    // metadata-table gate asks of a NEW site: the allowlist is a migration
    // ledger that only ever shrinks, so adding a row (and raising the
    // interpreter's per-needle budget with it) would have inverted the ratchet
    // this very decision is meant to respect.
    //
    // Same walk, same answer, and it populates the per-VM `LinkResolver` with
    // the resolution the invoke about to happen will ask for anyway. `cm` is
    // passed in, per `declared_method`'s contract, so the read guard this
    // function already holds stays the lock the call site established.
    //
    // An unresolvable method is `false`, exactly as the bare walk's `None` was:
    // no `Code` means the bridge keeps the call.
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let Ok(scoped) = resolver.declared_method(&cm, resolver.scope(start), method_name, descriptor)
    else {
        return false;
    };
    let Ok((declaring_id, index)) = resolver.adopt(scoped) else {
        return false;
    };
    cm.class_store
        .get(declaring_id)
        .and_then(|declaring| declaring.methods.get(index as usize))
        .is_some_and(|method| method.code().is_some())
}

/// The enforcement dial (`CRATONVM_ENFORCE_NATIVE_SHADOW`), asked at a
/// dispatch door **other than** step 1.
///
/// # What was wrong
///
/// `env_cache::jdk_only_enforce_shadow_for` had exactly one live call site
/// tree-wide -- `resolve_step1_native`, a few hundred lines above -- which
/// `H17-2` §5 established by grep over the whole repository rather than by
/// inference from behaviour. So "arming a class" armed only the subset of that
/// class's dispatches that reached step 1 **cold**. A dispatch served by a warm
/// invoke-cache entry, or by `invoke_or_native`'s registry-first probe, or by
/// that probe's superclass walk, ran the native regardless of the dial.
///
/// That is not a small discrepancy in a measurement. Four records
/// (`H0-3`, `H0-4`, `H14-3`, `H15`) priced retirements with this instrument,
/// and a retirement removes the *registration*, so under a retirement **every**
/// door misses. The dial priced a hybrid no retirement can reach: `H16-3`
/// photographed a real `Node[]` holding one real node and two fabrications.
/// The asymmetry that survives is the one to quote: **an armed FAILURE is real;
/// an armed ZERO is unreliable.**
///
/// # What this answers
///
/// The same three-way question step 1 asks, in the same order, with the
/// cheapest test first so that the doors this is called from -- one of which is
/// the hottest native path in the VM -- pay a `Copy` field read and an enum
/// compare on a default `--real-jdk` run and nothing else:
///
/// 1. `Bridge` only. `Intrinsic` is §1.4's reviewed exception and is taken
///    regardless; `SyntheticStub` is refused by §1.3 before any of this.
/// 2. `--jdk-only` only. The dial has never had meaning in `Compatible` mode
///    and must not acquire one here.
/// 3. The dial must cover THIS receiver class. `jdk_only_enforce_shadow()`
///    alone answers "is anything armed", which under a prefix list is true for
///    every class in the VM -- asking it here would enforce one subsystem's
///    dial across the whole process, which is the 32/17 -> 3/46 collapse the
///    scoping exists to avoid.
/// 4. And only then the hierarchy walk, which is the expensive part.
///
/// # The conservatism is deliberate and is NOT a full retirement
///
/// A yield needs concrete bytecode to yield *to*. A real retirement of a
/// registration whose method has no `Code` produces an `AbstractMethodError` or
/// a `MissingNative`; this dial produces the native, exactly as step 1 always
/// has. So an armed run still under-prices a retirement of a triple with no
/// bytecode behind it, and it must be read that way. What it no longer does is
/// under-price by an unknown factor that depends on which door the call
/// happened to arrive through.
///
/// # Per-call-site drift, the hazard this file has already paid for once
///
/// A `java/lang/String` force-native arm was deleted on 2026-08-04 because "a
/// method's behaviour started depending on how many times its call site had
/// run", and `forced_native_string_arm_stays_deleted` in this module is the
/// gate that keeps it deleted. Consulting a dial at a **memoized** door
/// recreates that defect unless the memo is dial-aware.
///
/// It is handled here by asking on every dispatch rather than at publication:
/// `revalidate_cached_native` calls this on every warm hit, not once at fill
/// time, and answers `None` (the eviction signal every caller already
/// implements) when the dial yields; `populate_invoke_cache` calls it before
/// deciding what to cache, so an armed class's bridge is never published to a
/// call site in the first place. Cold and warm therefore give the same answer
/// for the same triple, which is the property whose absence was the 2026-08-04
/// bug.
#[inline]
pub(crate) fn jdk_only_dial_yields_to_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    if kind != cratonvm_native_api::NativeKind::Bridge {
        return false;
    }
    if !crate::vm::dispatch_policy(shared).is_jdk_only() {
        return false;
    }
    if !crate::runtime::env_cache::jdk_only_enforce_shadow_for(class_name) {
        return false;
    }
    dial_yields_to_bytecode_slow(shared, class_name, method_name, descriptor)
}

/// [`jdk_only_dial_yields_to_bytecode`]'s tail, out of line so the three
/// early-outs above can inline into the hot doors without dragging a
/// hierarchy walk in with them.
#[cold]
#[inline(never)]
fn dial_yields_to_bytecode_slow(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    dispatch_has_code(shared, class_name, method_name, descriptor, None, true)
}

/// Resolve the complete identity stored by a warmed native invoke target.
///
/// `find` alone discards both the stable registry slot and `NativeKind`, which
/// used to force cache hits back through constant-pool resolution, a
/// class-manager lock and a second triple hash. Keep all three values together
/// at population time so the steady state remains genuinely O(1).
#[inline]
pub(super) fn resolve_cached_native_registration(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    cratonvm_native_api::NativeMethodId,
    cratonvm_native_api::NativeKind,
)> {
    let registry = &shared.natives.native_methods;
    let id = registry.resolve_id(class_name, method_name, descriptor)?;
    Some((registry.callback_of(id)?, id, registry.kind_of_id(id)?))
}

/// Revalidate and count a warmed native target without name-based lookup.
///
/// Compatible mode is the former direct-callback path plus one relaxed census
/// increment. Both modes redeem the current callback and kind from the stable
/// slot, preserving last-registration-wins; strict mode then routes the
/// decision through the central §7 policy. The name triple is materialized
/// only in strict mode, solely for a diagnostic if the live slot is refused.
#[inline]
pub(super) fn revalidate_cached_native(
    shared: &SharedVm,
    id: cratonvm_native_api::NativeMethodId,
    cached_callback: cratonvm_native_api::NativeCallback,
    cached_kind: cratonvm_native_api::NativeKind,
) -> Option<cratonvm_native_api::NativeCallback> {
    let registry = &shared.natives.native_methods;
    let policy = crate::vm::dispatch_policy(shared);

    // Native slots are updated in place on re-registration. Redeeming both
    // values before the compatible fast return prevents any warmed entry from
    // defeating last-write-wins while retaining indexed O(1) access.
    let callback = registry.callback_of(id).unwrap_or(cached_callback);
    let kind = registry.kind_of_id(id).unwrap_or(cached_kind);
    if !policy.is_jdk_only() {
        // REVALIDATE MEANS REVALIDATE. A warmed target is published once and
        // then redeemed forever, and until 2026-08-05 the Compatible arm
        // redeemed it without ever re-asking whether a `SyntheticStub` should
        // now yield to real bytecode. The arbitration answers "yield" every
        // time it is ASKED — measured, on both an isolated probe and a Spring
        // Boot run — and the census still counted 2 889
        // `AtomicBoolean.compareAndSet` stub dispatches, because the warmed
        // path is the one that never asked.
        //
        // `AtomicBoolean` and `java/time/Instant` are on
        // `real_protected_stub_class_common`'s allow-list, so the stated policy
        // is that their real bytecode wins once loaded. A target published
        // before that class finished loading pinned the stub for the rest of
        // the run.
        //
        // Returning `None` is the eviction signal every caller already
        // implements (`invoke_cache.evict(...)` then `CacheMiss`), so the site
        // re-resolves through a path that does arbitrate. Cost on the hot path
        // is one integer compare: only a `SyntheticStub` gets as far as
        // materialising its triple, and only the eleven allow-listed classes
        // reach the class manager.
        if kind == cratonvm_native_api::NativeKind::SyntheticStub {
            if let Some((class_name, method_name, descriptor)) = registry.triple_of(id) {
                if synthetic_stub_kind_should_yield_to_real_bytecode(
                    shared,
                    class_name,
                    method_name,
                    descriptor,
                    Some(kind),
                ) {
                    return None;
                }
            }
        }
        registry.record_invocation(id);
        return Some(callback);
    }

    let (class_name, method_name, descriptor) = registry.triple_of(id)?;
    match crate::vm::resolve_native_dispatch_wave1(
        crate::vm::DispatchDoor::CacheRevalidate,
        policy,
        class_name,
        method_name,
        descriptor,
        Some((callback, kind)),
        // A published native target has already won this call site's
        // compatibility decision.
        true,
        // Concrete bytecode precedence was decided before publication. If
        // redefinition can change that fact, the RedefineGate is checked and
        // evicts this target before we get here.
        //
        // ...with ONE exception, and it is the whole of `H17-3` §6: the
        // enforcement dial is process-static configuration that was not
        // consulted at publication either, because until now nothing but step 1
        // consulted it anywhere. A warmed entry published before the dial was
        // read would otherwise pin the bridge for the life of the process,
        // which is precisely how an armed run came to price a hybrid. Asking
        // here -- on every warm hit, not once at fill time -- is what makes the
        // warm and cold answers agree for the same triple. `None` from the
        // resolver is the eviction signal every caller of this function already
        // implements, so the site re-resolves through step 1, which asks the
        // same question and reaches the same bytecode.
        jdk_only_dial_yields_to_bytecode(shared, class_name, method_name, descriptor, kind),
    ) {
        Some(decision) => {
            let callback = decision.native_callback()?;
            registry.record_invocation(id);
            Some(callback)
        }
        None => None,
    }
}

#[cfg(test)]
mod enforcement_dial_door_tests {
    /// The dial must be consulted at every door that can run a `Bridge` in
    /// front of concrete bytecode -- not just at step 1.
    ///
    /// # Why this is a source scan and not a behavioural test
    ///
    /// `H17-2` §5 established the defect BY GREP, not by inference: on
    /// 2026-08-21 `jdk_only_enforce_shadow_for` had exactly one live call site
    /// in the whole repository, inside `resolve_step1_native`. A dispatch that
    /// does not pass through that line cannot be affected by the environment
    /// variable, in any mode, ever -- so the regression this guards against is
    /// structural, and the instrument that found it was structural. A
    /// behavioural test would need a VM with a real class library, a
    /// process-global env var read through a memoised slot, and a Java-visible
    /// witness -- four of the six this directory has reached for are already
    /// measured blind. This needs none of them and cannot go blind.
    ///
    /// # What the doors cost, MEASURED
    ///
    /// Instrumented build, `DialWitness direct`, armed for `java/util/HashMap`,
    /// one case per process (`H0-8` rule 1):
    ///
    /// ```text
    ///   [DIAL_DOOR_CENSUS] armed=true reached=947 yielded=57 leaked=890
    ///   [DIAL_DOOR] step1             reached=15   yielded=15   leaked=0
    ///   [DIAL_DOOR] invoke_or_native  reached=115  yielded=0    leaked=115
    ///   [DIAL_DOOR] force_intercept   reached=28   yielded=28   leaked=0
    ///   [DIAL_DOOR] cache_revalidate  reached=774  yielded=0    leaked=774
    ///   [DIAL_DOOR] cache_populate    reached=1    yielded=0    leaked=1
    ///   [DIAL_DOOR] jit_fast_native   reached=3    yielded=3    leaked=0
    ///   [DIAL_DOOR] stackless_force   reached=11   yielded=11   leaked=0
    /// ```
    ///
    /// **94% of armed `Bridge` dispatches never asked the dial**, and on a hot
    /// workload (`DialWitness jit`, 60 000 iterations) it was 299 431 of
    /// 299 469 -- so a suite, which is a hot workload, was very nearly unarmed.
    /// The three doors with a non-zero `leaked` column are the three pinned
    /// here. `force_intercept`, `stackless_force`, `jit_fast_native` and
    /// `elidable_ctor` pass `bytecode_available: true` unconditionally under
    /// `--jdk-only` and were already stricter than the dial, which is why they
    /// are absent.
    #[test]
    fn every_leaking_door_asks_the_enforcement_dial() {
        // (source, what it is, anchor proving the scan reads the right text,
        // how many QUALIFIED dial consultations must be there)
        //
        // The needle is the QUALIFIED path, so neither the definition (in this
        // file, unqualified) nor a doc-comment mention (inside backticks, no
        // `(`) can inflate the count.
        let doors: &[(&str, &str, &str, usize)] = &[
            (
                include_str!("../../vm/vm_exec.rs"),
                "vm/src/vm/vm_exec.rs :: invoke_or_native -- the registry-first probe, its \
                 array-type alias retry, and both arms of its superclass walk",
                "fn invoke_or_native",
                4,
            ),
            (
                include_str!("dispatch_static.rs"),
                "vm/src/runtime/interpreter/dispatch_static.rs :: populate_invoke_cache",
                "fn populate_invoke_cache",
                1,
            ),
        ];
        for (src, what, anchor, want) in doors {
            // Establish the corpus is real before concluding anything from a
            // count: a file that stopped containing the door at all would make
            // this pass, or fail, for the wrong reason.
            assert!(
                src.contains(anchor),
                "{what}: the anchor `{anchor}` is gone, so this scan is looking at the wrong \
                 text and whatever it counts means nothing"
            );
            let calls = src
                .lines()
                .filter(|l| {
                    let t = l.trim_start();
                    !t.starts_with("//") && t.contains("::jdk_only_dial_yields_to_bytecode(")
                })
                .count();
            assert!(
                calls >= *want,
                "{what} consults the enforcement dial at {calls} site(s), expected at least \
                 {want}. `CRATONVM_ENFORCE_NATIVE_SHADOW` is meant to simulate a retirement, \
                 and a retirement removes the REGISTRATION -- so every door misses, not just \
                 the cold step-1 one. A door that stops asking silently returns the dial to \
                 the state H17-2 measured: armed failures real, armed zeros unreliable, and \
                 every armed cell in docs/known-issues/jdk-only/ pricing a hybrid that no \
                 retirement can reach."
            );
        }
    }

    /// The warm invoke-cache door -- this file's own, and the one that carried
    /// 82% of the leak.
    ///
    /// Scoped to `revalidate_cached_native`'s body rather than counted over the
    /// file, because the needle is unqualified here and this module's own
    /// source would otherwise satisfy the search it performs.
    #[test]
    fn the_warm_invoke_cache_door_asks_the_enforcement_dial() {
        let src = include_str!("native_override.rs");
        let start = src
            .find("pub(super) fn revalidate_cached_native(")
            .expect("`revalidate_cached_native` is gone, so this scan reads the wrong text");
        let rest = &src[start..];
        // The next `#[cfg(test)]` is this very module; everything between is
        // the function and the plain items that follow it.
        let end = rest.find("\n#[cfg(test)]").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("jdk_only_dial_yields_to_bytecode("),
            "`revalidate_cached_native` no longer asks the enforcement dial. It is the door a \
             warmed native target is redeemed through, and it carried 774 of the 890 leaked \
             armed dispatches in the measurement quoted above -- 299 292 of 299 431 on a hot \
             one. A warmed entry published before the dial was consulted pins the bridge for \
             the life of the process, which is exactly how an armed run came to price a \
             hybrid."
        );
    }

    /// Every production file that forces a native either asks the dial, or is
    /// named here with a reason.
    ///
    /// # Why a source scan, and not a counter
    ///
    /// `enforcement_dial` in `--jdk-only-report` counts a door only when that
    /// door calls `note_dial_door`. A door that calls neither the dial nor the
    /// census is therefore **invisible to the census**: `reached` does not
    /// rise, `declined_no_bytecode` does not rise, and an armed report reads
    /// exactly as it does when every door is wired. **Counting cannot find a
    /// missing counter.**
    ///
    /// That was not hypothetical. On 2026-08-22, after the fourteen-door fix,
    /// `dispatch_virtual.rs` and `jit_bridge.rs` were each still forcing
    /// natives through `force_native_over_real_jdk_bytecode` with ZERO dial
    /// calls and ZERO census calls anywhere in the file — while the armed
    /// report showed `reached == yielded` and nothing amiss. Lane WORKER 2 hit
    /// the same gap from the other end: an armed `ConcurrentHashMap` dropping
    /// stores and calling each one a fresh insert.
    ///
    /// So this gate is structural. Adding a new undialled force path is a RED
    /// TEST rather than something the next lane discovers from a corrupted map.
    #[test]
    fn every_force_native_file_asks_the_dial_or_is_exempt() {
        // (file, permanent?, reason).
        //
        // `permanent == false` is a HOLE that has been written down, and the
        // count of those is what should reach zero. `permanent == true` is a
        // file where the dial is structurally not applicable — it still calls
        // the helper, so the scan must account for it, but it is not
        // outstanding work. Filing both under one heading makes the list read
        // as twice the remaining problem.
        const FORCE_SITES_EXEMPT: &[(&str, bool, &str)] = &[(
            "jit_bridge.rs",
            true,
            "NOT APPLICABLE, verified by reading the bind path rather than \
                 inferred from the grep that first listed it. Under `--jdk-only`, \
                 `jit::direct_native_helper` refuses to bind any native whose \
                 registry kind is not `Intrinsic` (§1.4's reviewed exception) and \
                 records the refusal. The dial's entire domain is `Bridge` under \
                 `--jdk-only`, a strict SUBSET of what that already refuses, so \
                 there is no configuration in which the dial would yield a native \
                 the JIT would otherwise bind. What this file does with the force \
                 helper is decide whether to SEAL a caller out of tier-up; missing \
                 the dial there over-seals an armed run, which costs tier-up in \
                 the safe direction. Wiring it is a tier-up optimisation, not a \
                 correctness fix — and `registered_native_will_run`, the natural \
                 place, also feeds interpreter dispatch, so it is not a free edit.",
        )];

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }

        fn calls_force(src: &str) -> bool {
            src.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains("force_native_over_real_jdk_bytecode("))
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);

        let mut offenders: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for f in &files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            // `tests.rs` asserts the force TABLE's contents and dispatches
            // nothing; `native_override.rs` defines both the helper and the dial.
            if name == "tests.rs" || name == "native_override.rs" {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(f) else {
                continue;
            };
            if !calls_force(&src) {
                continue;
            }
            checked += 1;
            let asks = src.contains("jdk_only_dial_yields_to_bytecode")
                || src.contains("enforce_shadow_scope()");
            let exempt = FORCE_SITES_EXEMPT.iter().any(|(n, _, _)| *n == name);
            if !asks && !exempt {
                offenders.push(name);
            }
        }

        assert!(
            checked > 0,
            "no file under vm/src calls `force_native_over_real_jdk_bytecode(`, so this scan \
             is searching for a spelling that no longer exists and would pass on a tree with \
             no force gate at all"
        );
        assert!(
            offenders.is_empty(),
            "{offenders:?} force natives through `force_native_over_real_jdk_bytecode` without \
             ever consulting the enforcement dial, and without a row in FORCE_SITES_EXEMPT.\n\n\
             The `enforcement_dial` report CANNOT catch this: a door that never calls \
             `note_dial_door` does not raise `reached`, so an armed run looks identical to a \
             correctly-wired one. That is how these two files stayed undialled through a lane \
             that measured 890 of 947 leaked dispatches and believed it had closed all of \
             them.\n\n\
             Either consult the dial at the force site, or add a row with the reason."
        );

        // The exemption list must not rot, in EITHER direction, and the second
        // direction is the one that actually happens.
        //
        //   * a row naming a file that no longer forces anything is a stale
        //     approval;
        //   * a row naming a file that has since been WIRED is worse — it is a
        //     standing approval for a hole somebody already filled, and nothing
        //     would ever remove it.
        //
        // With both checked, this list can only shrink: wiring a force site
        // turns the gate red until its row is deleted.
        // ZERO, and held there. Every force site under vm/src now either consults
        // the dial or is a reasoned `permanent: true` non-hole. A new unwired
        // site is a red test on its own — nobody has to notice a count creep up.
        let holes = FORCE_SITES_EXEMPT
            .iter()
            .filter(|(_, perm, _)| !*perm)
            .count();
        assert_eq!(
            holes, 0,
            "{holes} force site(s) are UNWIRED holes. A `permanent: true` row is a \
             reasoned non-hole and does not count toward this. If a hole genuinely has \
             to exist for a while, write the reason and the retiring condition in the \
             row and raise this bound deliberately — do not flip the flag to true, \
             which is what turns a to-do into a permanent approval."
        );

        for (name, _, _) in FORCE_SITES_EXEMPT {
            let found = files
                .iter()
                .find(|f| f.file_name().unwrap().to_string_lossy() == *name)
                .and_then(|f| std::fs::read_to_string(f).ok());
            let Some(src) = found else {
                panic!(
                    "FORCE_SITES_EXEMPT names `{name}`, which is not a file under vm/src. \
                     Drop the row — a row that matches nothing reads as a reviewed hole."
                );
            };
            assert!(
                calls_force(&src),
                "FORCE_SITES_EXEMPT names `{name}`, which no longer calls the force helper. \
                 Drop the row — a stale exemption reads as a reviewed hole."
            );
            assert!(
                !(src.contains("jdk_only_dial_yields_to_bytecode")
                    || src.contains("enforce_shadow_scope()")),
                "FORCE_SITES_EXEMPT still names `{name}`, but it NOW CONSULTS THE DIAL. \
                 Delete the row. An exemption that outlives the hole it excused is a \
                 standing approval nobody will ever revisit, and it makes the list read \
                 as bigger than the remaining problem."
            );
        }
    }

    /// The scans above can fail.
    ///
    /// A guard that cannot fail reads exactly like one that works; this file
    /// already carries a note about three such guards found in one evening. If
    /// the helper is renamed, every count above silently drops to zero and the
    /// assertions become a report about a string that appears nowhere -- so pin
    /// the spelling against its definition site.
    #[test]
    fn the_door_scans_are_looking_for_a_name_that_exists() {
        let src = include_str!("native_override.rs");
        assert!(
            src.contains("pub(crate) fn jdk_only_dial_yields_to_bytecode("),
            "`jdk_only_dial_yields_to_bytecode` was renamed or removed, so the two door scans \
             above now search for a string that appears nowhere and would pass on a tree with \
             no dial wiring at all"
        );
    }
}

#[cfg(test)]
mod string_builder_layout_override_tests {
    use super::is_string_builder_layout_native_override;

    /// Every builder operation whose real JDK body indexes the compact
    /// `byte[] value` / `byte coder` / `int count` layout must resolve through
    /// CratonVM's native shim, because CratonVM's builders are a two-field
    /// `char[]`/`int` synthetic. Before 2026-07-31 six of these were missing,
    /// and a single `Mockito.mock(StringBuilder.class)` anywhere in the process
    /// evicted their native shadow and silently corrupted every REAL builder —
    /// which is what wedged javac's `JavaTokenizer` and made Spring's AOT
    /// chunk 4 look like a hang. `SbMethodMatrixProbe` is the Java witness.
    #[test]
    fn forces_every_compact_layout_operation_native() {
        for class in ["java/lang/StringBuilder", "java/lang/AbstractStringBuilder"] {
            for (name, desc) in [
                ("setLength", "(I)V"),
                ("deleteCharAt", "(I)Ljava/lang/StringBuilder;"),
                ("replace", "(IILjava/lang/String;)Ljava/lang/StringBuilder;"),
                ("ensureCapacity", "(I)V"),
                ("trimToSize", "()V"),
                ("repeat", "(II)Ljava/lang/StringBuilder;"),
                ("capacity", "()I"),
                ("reverse", "()Ljava/lang/StringBuilder;"),
                ("codePointAt", "(I)I"),
                ("codePointBefore", "(I)I"),
                ("codePointCount", "(II)I"),
                ("getCoder", "()B"),
                ("getValue", "()[B"),
                // Already covered before this fix; kept so a future narrowing
                // of the list is caught here too.
                ("<init>", "(Ljava/lang/String;)V"),
                ("append", "(C)Ljava/lang/StringBuilder;"),
                ("charAt", "(I)C"),
                ("delete", "(II)Ljava/lang/StringBuilder;"),
                ("getChars", "(II[CI)V"),
                ("insert", "(IC)Ljava/lang/StringBuilder;"),
                ("setCharAt", "(IC)V"),
                ("toString", "()Ljava/lang/String;"),
                ("substring", "(II)Ljava/lang/String;"),
            ] {
                assert!(
                    is_string_builder_layout_native_override(class, name, desc),
                    "{class}.{name}{desc} must stay forced to its native shim"
                );
            }
        }
    }

    /// The two operations `MockitoBeanByTypeLookupIntegrationTests` genuinely
    /// stubs and verifies on a mocked `StringBuilder`. Their native shadow has
    /// to stay evictable or Mockito's woven advice never runs and the stub is
    /// silently ignored — see the long note on `length()` in the predicate.
    #[test]
    fn leaves_the_two_mockito_stubbed_operations_evictable() {
        for class in ["java/lang/StringBuilder", "java/lang/AbstractStringBuilder"] {
            assert!(!is_string_builder_layout_native_override(
                class, "length", "()I"
            ));
            assert!(!is_string_builder_layout_native_override(
                class,
                "substring",
                "(I)Ljava/lang/String;"
            ));
        }
    }

    /// `java/lang/StringBuffer` is claimed by NOTHING here, and that is the
    /// paired half of retiring its 62 registrations: its own `synchronized`
    /// bodies must run, or the buffer loses both its monitor and its
    /// `toStringCache` invalidation. Asserted operation by operation rather
    /// than once, so a future widening of the list cannot quietly re-take the
    /// class.
    #[test]
    fn never_claims_string_buffer_whose_own_bodies_carry_the_monitor() {
        for (name, desc) in [
            ("append", "(Ljava/lang/String;)Ljava/lang/StringBuffer;"),
            ("append", "(C)Ljava/lang/StringBuffer;"),
            ("insert", "(ILjava/lang/String;)Ljava/lang/StringBuffer;"),
            ("delete", "(II)Ljava/lang/StringBuffer;"),
            ("deleteCharAt", "(I)Ljava/lang/StringBuffer;"),
            ("replace", "(IILjava/lang/String;)Ljava/lang/StringBuffer;"),
            ("reverse", "()Ljava/lang/StringBuffer;"),
            ("setLength", "(I)V"),
            ("setCharAt", "(IC)V"),
            ("toString", "()Ljava/lang/String;"),
            ("charAt", "(I)C"),
            ("length", "()I"),
            ("capacity", "()I"),
            ("ensureCapacity", "(I)V"),
            ("trimToSize", "()V"),
            ("getChars", "(II[CI)V"),
            ("substring", "(II)Ljava/lang/String;"),
            ("<init>", "(Ljava/lang/String;)V"),
        ] {
            assert!(
                !is_string_builder_layout_native_override("java/lang/StringBuffer", name, desc),
                "StringBuffer.{name}{desc} must run its OWN synchronized body"
            );
        }
    }

    #[test]
    fn does_not_claim_unrelated_classes() {
        assert!(!is_string_builder_layout_native_override(
            "java/lang/String",
            "setLength",
            "(I)V"
        ));
        assert!(!is_string_builder_layout_native_override(
            "java/util/ArrayList",
            "trimToSize",
            "()V"
        ));
    }
}

#[cfg(test)]
mod redefine_immunity_tests {
    use super::redefine_immune_forced_native;

    /// CratonVM's synthetic collections must keep their native shadow across a
    /// redefinition. Their real JDK bodies index a `table`/`root`/`head` field
    /// graph the synthetic objects do not have, so running them returns silent
    /// nonsense — `TreeMap.get` null, `ConcurrentHashMap.size` 0,
    /// `HashMap.keySet` empty. `RedefineCollectionLayoutProbe` is the witness.
    #[test]
    fn synthetic_collections_keep_their_natives_across_a_redefinition() {
        for class in [
            "java/util/ArrayDeque",
            "java/util/ArrayList",
            "java/util/HashMap",
            "java/util/HashSet",
            "java/util/IdentityHashMap",
            "java/util/LinkedHashMap",
            "java/util/LinkedHashSet",
            "java/util/LinkedList",
            "java/util/TreeMap",
            "java/util/TreeSet",
            "java/util/concurrent/ConcurrentHashMap",
        ] {
            for (name, desc) in [
                ("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
                ("size", "()I"),
                ("containsKey", "(Ljava/lang/Object;)Z"),
                ("keySet", "()Ljava/util/Set;"),
                ("entrySet", "()Ljava/util/Set;"),
                ("iterator", "()Ljava/util/Iterator;"),
                ("toString", "()Ljava/lang/String;"),
            ] {
                assert!(
                    redefine_immune_forced_native(class, name, desc),
                    "{class}.{name}{desc} must survive a redefinition"
                );
            }
        }
    }

    /// `ThreadLocal`'s values live in a Rust thread-local keyed by identity
    /// hash, not in `Thread.threadLocals`, so its real JDK body answers `null`
    /// for every value the process ever set. Mockito instruments the whole
    /// superclass chain, so mocking ANY `ThreadLocal` subclass retransforms
    /// `java.lang.ThreadLocal` and arms it — measured at
    /// `ThreadLocal.get` native calls 23 -> 4 with the mock target as the only
    /// variable. `probes/ThreadLocalRetransformProbe.java` is the witness.
    #[test]
    fn thread_local_keeps_its_natives_across_a_redefinition() {
        for class in ["java/lang/ThreadLocal", "java/lang/InheritableThreadLocal"] {
            for (name, desc) in [
                ("<init>", "()V"),
                ("get", "()Ljava/lang/Object;"),
                ("set", "(Ljava/lang/Object;)V"),
                ("remove", "()V"),
                ("initialValue", "()Ljava/lang/Object;"),
                (
                    "withInitial",
                    "(Ljava/util/function/Supplier;)Ljava/lang/ThreadLocal;",
                ),
            ] {
                assert!(
                    redefine_immune_forced_native(class, name, desc),
                    "{class}.{name}{desc} must survive a redefinition"
                );
            }
        }
    }

    /// Every operation CratonVM forces to its `ZipFile`/`JarFile` native must
    /// survive a redefinition of those two classes on the SLOW-path
    /// aggregator, and must NOT be immune on the invoke-cache one. One `spy()`
    /// of a `JarFile` subclass retransforms the whole chain, and the real
    /// bodies read a `res`/`zsrc` field graph no CratonVM archive has — but a
    /// Mockito inline MOCK of the same class has no archive handle either, and
    /// only the slow path can tell the two apart. See
    /// `redefine_immune_layout_native`'s note on why this arm is the one that
    /// must differ between the aggregators.
    #[test]
    fn leaves_the_zip_arm_to_the_receiver_aware_slow_path() {
        for name in [
            "<init>",
            "getEntry",
            "getInputStream",
            "entries",
            "stream",
            "getComment",
            "close",
            "getName",
            "isMultiRelease",
            "size",
        ] {
            assert!(
                redefine_immune_forced_native("java/util/zip/ZipFile", name, "()V"),
                "java/util/zip/ZipFile.{name} must survive a redefinition"
            );
            assert!(
                !super::redefine_immune_layout_native("java/util/zip/ZipFile", name, "()V"),
                "java/util/zip/ZipFile.{name} must NOT be immune on the invoke-cache \
                 path: that path cannot tell a real archive from a mock of the same \
                 class, so it has to defer to the receiver-aware slow path"
            );
        }
        for name in [
            "<init>",
            "getManifest",
            "getManifestFromReference",
            "stream",
            "entries",
            "getEntry",
            "getJarEntry",
            "getInputStream",
            "size",
            "close",
            "getName",
        ] {
            assert!(
                redefine_immune_forced_native("java/util/jar/JarFile", name, "()V"),
                "java/util/jar/JarFile.{name} must survive a redefinition"
            );
            assert!(
                !super::redefine_immune_layout_native("java/util/jar/JarFile", name, "()V"),
                "java/util/jar/JarFile.{name} must NOT be immune on the invoke-cache \
                 path: see the ZipFile arm above"
            );
        }
    }

    /// The immunity is method-wise, and its boundary is the force-native list:
    /// an operation CratonVM does NOT claim is ordinary bytecode and must stay
    /// evictable, or an agent could never weave it. A `JarFile` SUBCLASS is
    /// ordinary throughout -- its own bodies are its own bytecode, and
    /// `NestedJarFile.getInputStream` is exactly such an override.
    #[test]
    fn leaves_unclaimed_zip_operations_evictable() {
        for (class, name) in [
            ("java/util/jar/JarFile", "getVersion"),
            ("java/util/jar/JarFile", "isSigned"),
            ("java/util/zip/ZipFile", "getComment2"),
            (
                "org/springframework/boot/loader/jar/NestedJarFile",
                "getInputStream",
            ),
        ] {
            assert!(
                !redefine_immune_forced_native(class, name, "()V"),
                "{class}.{name} must stay evictable"
            );
        }
    }

    /// The two aggregators are consulted by different dispatch paths — the
    /// invoke-cache sites use `redefine_immune_layout_native`, everything else
    /// `redefine_immune_forced_native`. An arm added to one only is the failure
    /// the collection entry already made once: the probe went from 32 broken
    /// operations to 18 instead of to 0, because the cache sites re-assembled
    /// their own chain and never saw it. So assert the layout aggregator
    /// directly rather than trusting that both were edited.
    #[test]
    fn thread_local_immunity_reaches_the_invoke_cache_sites_too() {
        for class in ["java/lang/ThreadLocal", "java/lang/InheritableThreadLocal"] {
            assert!(
                super::redefine_immune_layout_native(class, "get", "()Ljava/lang/Object;"),
                "{class}.get must be immune on the invoke-cache path as well"
            );
        }
    }

    /// A VM-minted carrier has no bytecode in any image, so the "yield to the
    /// woven body" rule has nothing to yield TO -- `find_method_recursive` walks
    /// past it to `java/lang/Object`, whose generation Mockito bumps the first
    /// time anything mocks a class. Measured: without this arm, one `mock()`
    /// made every `Collections.unmodifiableList` / `List.of` in the process
    /// compare by identity.
    #[test]
    fn vm_minted_carriers_keep_their_natives_across_a_redefinition() {
        for class in [
            "cratonvm/internal/UnmodifiableList",
            "cratonvm/internal/UnmodifiableSet",
            "cratonvm/internal/UnmodifiableMap",
            "cratonvm/internal/UnmodifiableCollection",
            "cratonvm/internal/UnmodifiableSortedSet",
            "cratonvm/internal/UnmodifiableNavigableSet",
            "cratonvm/internal/UnmodifiableEntrySet",
            "cratonvm/internal/UnmodifiableMapEntry",
            "cratonvm/internal/ArrayListSubList",
            "cratonvm/internal/foreign/MemorySegmentImpl",
        ] {
            for (name, desc) in [
                ("equals", "(Ljava/lang/Object;)Z"),
                ("hashCode", "()I"),
                ("size", "()I"),
                ("toString", "()Ljava/lang/String;"),
            ] {
                assert!(
                    redefine_immune_forced_native(class, name, desc),
                    "{class}.{name}{desc} must survive a redefinition"
                );
            }
        }
    }

    /// Same arm, the other aggregator -- the failure
    /// `thread_local_immunity_reaches_the_invoke_cache_sites_too` exists to
    /// catch, made once already by the collection arm.
    #[test]
    fn vm_minted_carrier_immunity_reaches_the_invoke_cache_sites_too() {
        assert!(super::redefine_immune_layout_native(
            "cratonvm/internal/UnmodifiableList",
            "equals",
            "(Ljava/lang/Object;)Z"
        ));
    }

    #[test]
    fn ordinary_classes_stay_evictable() {
        assert!(!redefine_immune_forced_native(
            "com/example/Service",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ));
        // The prefix is `cratonvm/internal/`, not `cratonvm`: an application
        // class that merely starts with the vendor word is an ordinary class.
        assert!(!redefine_immune_forced_native(
            "cratonvm/app/Service",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ));
        // A ThreadLocal SUBCLASS is an ordinary class: its own methods are its
        // own bytecode, dispatch resolves to them before this gate is reached,
        // and a mock of one must still be able to intercept them. Only the two
        // JDK classes whose bodies CratonVM replaces are immune.
        assert!(!redefine_immune_forced_native(
            "org/springframework/core/NamedThreadLocal",
            "get",
            "()Ljava/lang/Object;"
        ));
        // The two operations the Mockito suite genuinely stubs on a mocked
        // StringBuilder — see `is_string_builder_layout_native_override`.
        assert!(!redefine_immune_forced_native(
            "java/lang/StringBuilder",
            "length",
            "()I"
        ));
        assert!(!redefine_immune_forced_native(
            "java/lang/StringBuilder",
            "substring",
            "(I)Ljava/lang/String;"
        ));
    }

    /// No dispatch site may name a layout arm directly: they must go through
    /// `redefine_immune_layout_native` (cache paths) or
    /// `redefine_immune_forced_native` (slow path). Six sites used to open-code
    /// `string_builder || path`, so the collections entry silently did not apply
    /// there and the collection probe stalled at 18 of 32 rather than 0.
    ///
    /// The reflection arm is NOT policed: two cache sites legitimately omit it,
    /// and forcing them to include it regressed ByteBuddy type creation.
    #[test]
    fn layout_immunity_is_not_open_coded() {
        // The aggregators this gate polices moved out of  with the
        // rest of the override policy in the SEAM-02 split, and this test moved
        // with them. Scan the file they are actually in — which is this one.
        //
        // A third way for this gate to go stale, after the hard-coded line band
        // and the declaration parser described above: pointing at a file the
        // subject has left. That fails CLOSED here (the aggregator lookup finds
        // nothing and the assertion below fires), which is the right direction,
        // and is how the split found it.
        // Normalise line endings before ANY byte-offset arithmetic below.
        // `include_str!` embeds the file's raw bytes and this repository is
        // checked out with CRLF on Windows (`core.autocrlf=true`). Two things
        // then go wrong, and the second is the one that bit:
        //
        //   * the `"\n}"` body terminator still matches (`\r\n}` contains
        //     `\n}`), so that one is fine either way; but
        //   * `offset += line.len() + 1` below assumes `lines()` stripped ONE
        //     byte. On CRLF it strips two, so `line_start` drifts a byte per
        //     line. By the aggregators (~line 4900) it is ~4900 bytes short of
        //     the true offset, the `inside_aggregator` range check misses, and
        //     the aggregators' OWN arms are reported as offenders — the gate
        //     failing for a reason that has nothing to do with what it
        //     polices, which is precisely the staleness mode the comment above
        //     was written to prevent. A fourth way to go stale, after the two
        //     it already lists.
        //
        // Same fix, same reason, as `jit::ir_lower`'s `declared_op_variants`
        // and `vm::runtime::env_cache`'s flags scan.
        // A FIFTH staleness mode, found 2026-08-06: this gate policed exactly
        // one file. The rule it states — "an arm may be named only inside the
        // two aggregators" — is a rule about the override policy, not about a
        // file, and the aggregators are `pub(super)`, so every sibling module
        // can name an arm and none of them were being read.
        //
        // `vm/src/runtime/interpreter.rs` did: its no-`Code` dispatch arm
        // hand-rolled `reflection && string_builder && path`, the aggregate
        // minus five arms including `synthetic_collection`. That is the same
        // open-coding whose 2026-07-31 instance took the collection probe to 18
        // rather than 0 — the incident written up on
        // `redefine_immune_forced_native` as the reason this gate exists. It
        // sat in a sibling file for as long as the gate has been green.
        //
        // Scanning the whole `runtime` subtree is the fix. `include_str!` needs
        // literal paths, so the sibling list is explicit; a new module that
        // names an arm is not covered until it is added here, which is the
        // remaining hole and is at least a hole in one obvious place.
        let sources: [(&str, String); 6] = [
            (
                "native_override.rs",
                include_str!("native_override.rs").replace("\r\n", "\n"),
            ),
            (
                "../interpreter.rs",
                include_str!("../interpreter.rs").replace("\r\n", "\n"),
            ),
            ("invoke.rs", include_str!("invoke.rs").replace("\r\n", "\n")),
            (
                "dispatch_virtual.rs",
                include_str!("dispatch_virtual.rs").replace("\r\n", "\n"),
            ),
            (
                "../instrument.rs",
                include_str!("../instrument.rs").replace("\r\n", "\n"),
            ),
            (
                "../../vm/vm_exec.rs",
                include_str!("../../vm/vm_exec.rs").replace("\r\n", "\n"),
            ),
        ];

        // The aggregators live in THIS file, so the exemption range is computed
        // from this file's text and applies only while scanning it.
        let src = sources[0].1.as_str();

        // The exemption is the RULE, located in the source: an arm may be named
        // only inside the two aggregators, whose entire job is to compose them.
        //
        // Twice now this gate has been written as a proxy for that rule, and
        // twice the proxy went stale against a growing file. First a hard-coded
        // line band, `(5000..9800)`: `redefine_immune_forced_native` slid down
        // to 9793-9822, so its own arms were reported as offenders and the gate
        // failed for a reason that had nothing to do with what it polices.
        // Then an enclosing-function tracker that recognised exactly four
        // declaration spellings — a function written any other way never
        // updated the name, so its body was attributed to whatever came before.
        // That one failed OPEN, which is worse: dropping
        //
        //     pub(super) unsafe fn probe(class_name: &str) -> bool {
        //         redefine_immune_synthetic_collection_native(class_name)
        //     }
        //
        // straight after `redefine_immune_synthetic_collection_native` PASSED,
        // because the stale name was that exempt predicate's.
        //
        // So: no line numbers, no declaration parsing, no carried state. Find
        // the aggregator bodies and ask whether the call is inside one. If an
        // aggregator is ever renamed this stops finding it and its own arms
        // start failing — loud, and the right direction to fail in.
        //
        // The list IS the exemption, and its LENGTH is the findability
        // check: a composer added here can never silently widen the gate's
        // blind spot without also being visible in this array.
        const EXEMPT_BODIES: [&str; 3] = [
            "redefine_immune_layout_native",
            "redefine_immune_forced_native",
            // The receiver-aware composer. It names the ZIP arm for the same
            // reason the two aggregators name their own: composing them IS its
            // job. What the gate forbids is a DISPATCH SITE open-coding an arm,
            // and this is not one — every dispatch site calls
            // `redefine_immune_forced_native_for_receiver`, never this.
            "zip_immunity_waived_for_receiver",
        ];
        let aggregator_bodies: Vec<(usize, usize)> = EXEMPT_BODIES
            .iter()
            .filter_map(|name| {
                let start = src.find(&format!("fn {name}("))?;
                // A top-level body ends at the first `}` in column 0 after it.
                let end = src[start..]
                    .find("\n}")
                    .map_or(src.len(), |i| start + i + 2);
                Some((start, end))
            })
            .collect();
        let missing: Vec<&str> = EXEMPT_BODIES
            .iter()
            .copied()
            .filter(|name| !src.contains(&format!("fn {name}(")))
            .collect();
        assert!(
            missing.is_empty() && aggregator_bodies.len() == EXEMPT_BODIES.len(),
            "every exempt body must be findable. Not found: {missing:?}"
        );

        let mut offenders = Vec::new();
        for (file, text) in &sources {
            // The aggregator exemption is a byte range in `native_override.rs`
            // only. In any other file there is nothing to exempt — naming an
            // arm there is the offence, wherever in the file it sits.
            let exempt: &[(usize, usize)] = if *file == "native_override.rs" {
                &aggregator_bodies
            } else {
                &[]
            };
            let mut offset = 0usize;
            for (n, line) in text.lines().enumerate() {
                let line_start = offset;
                offset += line.len() + 1; // `lines()` strips a single `\n`
                let code = line.trim_start();
                // Comments, and this test's own list of names (string literals).
                if code.starts_with("//") || code.starts_with('"') {
                    continue;
                }
                if exempt
                    .iter()
                    .any(|&(start, end)| line_start >= start && line_start < end)
                {
                    continue;
                }
                for part in [
                    "redefine_immune_string_builder_native(",
                    "redefine_immune_path_native(",
                    "redefine_immune_jfr_native(",
                    "redefine_immune_synthetic_collection_native(",
                    "redefine_immune_vm_minted_carrier_native(",
                    "redefine_immune_thread_local_native(",
                    "redefine_immune_zip_file_native(",
                ] {
                    // An arm's own `fn` declaration is not a call site.
                    if code.contains(part) && !code.contains(&format!("fn {part}")) {
                        offenders.push(format!("{}:{}: {}", file, n + 1, code));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "call redefine_immune_layout_native (cache paths) or \
             redefine_immune_forced_native (slow path) instead of naming an arm:\n{}",
            offenders.join("\n")
        );
    }
}

#[cfg(test)]
mod intercept_shape_tests {
    use super::{
        class_reflection_shape, classloader_resource_shape, http_carrier_declaring_class,
        intercept_shape_of, INTERCEPT_SHAPE_CLASSLOADER_RESOURCE, INTERCEPT_SHAPE_CLASS_REFLECTION,
        INTERCEPT_SHAPE_HTTP_CARRIER,
    };

    /// Every triple that any of the three arms can fire on MUST set its bit.
    ///
    /// This is the guard, and the failure it guards against is not a slow path:
    /// `intercept_force_registered_native_cached` skips an arm outright when the
    /// bit is clear, so an arm that grows a name its classifier does not know
    /// stops running. Both halves are derived from the same two helpers here, so
    /// the test is only meaningful together with
    /// `the_arms_use_the_shared_helpers` below — which is what pins that the
    /// dispatch code and the classifier read the SAME predicate rather than two
    /// copies that can drift.
    #[test]
    fn intercept_shape_agrees_with_the_arms_it_gates() {
        let classloader = [
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;"),
            (
                "getResources",
                "(Ljava/lang/String;)Ljava/util/Enumeration;",
            ),
            (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
            ),
        ];
        for (m, d) in classloader {
            assert!(classloader_resource_shape(m, d), "{m}{d}");
            let shape = intercept_shape_of("some/user/Loader", m, d);
            assert_ne!(
                shape & INTERCEPT_SHAPE_CLASSLOADER_RESOURCE,
                0,
                "{m}{d} must set the ClassLoader bit"
            );
        }

        let reflection = [
            ("getProtectionDomain", "()Ljava/security/ProtectionDomain;"),
            ("isArray", "()Z"),
            ("getComponentType", "()Ljava/lang/Class;"),
            ("componentType", "()Ljava/lang/Class;"),
        ];
        for (m, d) in reflection {
            assert!(class_reflection_shape(m, d), "{m}{d}");
            let shape = intercept_shape_of("java/lang/Class", m, d);
            assert_ne!(
                shape & INTERCEPT_SHAPE_CLASS_REFLECTION,
                0,
                "{m}{d} must set the reflection bit"
            );
        }

        let carriers = [
            "java/net/URLConnection",
            "java/net/HttpURLConnection",
            "javax/net/ssl/HttpsURLConnection",
            "sun/net/www/protocol/http/HttpURLConnection",
            "sun/net/www/protocol/https/HttpsURLConnectionImpl",
        ];
        for c in carriers {
            assert!(http_carrier_declaring_class(c), "{c}");
            let shape = intercept_shape_of(c, "getResponseCode", "()I");
            assert_ne!(
                shape & INTERCEPT_SHAPE_HTTP_CARRIER,
                0,
                "{c} must set the carrier bit"
            );
        }
    }

    /// The shape of an ordinary call site is ZERO — which is the whole point:
    /// that is the case the fast path exists for, and if some innocuous triple
    /// set a bit the memo would buy nothing.
    ///
    /// `int callee(int)` is `probes/InvokeAttributionProbe.java`'s own callee,
    /// i.e. the exact shape the 1.50% measurement was taken on.
    #[test]
    fn an_ordinary_call_site_reaches_none_of_the_arms() {
        for (c, m, d) in [
            ("InvokeAttributionProbe", "callee", "(I)I"),
            ("java/lang/String", "length", "()I"),
            ("java/util/ArrayList", "add", "(Ljava/lang/Object;)Z"),
            (
                "org/apache/tomcat/util/bcel/classfile/ConstantPool",
                "getConstant",
                "(ILjava/lang/Class;)Lorg/apache/tomcat/util/bcel/classfile/Constant;",
            ),
            // Right names, WRONG owner: the reflection arm is keyed on the
            // method pair alone, so this one legitimately sets a bit and the
            // receiver check below it is what declines. Recorded so a future
            // reader does not "tighten" the classifier by adding an owner test
            // the arm itself does not make.
        ] {
            assert_eq!(
                intercept_shape_of(c, m, d),
                0,
                "{c}.{m}{d} must not reach any special-case arm"
            );
        }
    }

    /// A near miss on each list must NOT set the bit — the classifier has to be
    /// exact, because a false positive silently reintroduces the per-call string
    /// work this change removed.
    #[test]
    fn near_misses_do_not_set_a_bit() {
        assert!(!classloader_resource_shape(
            "getResource",
            "(Ljava/lang/String;)Ljava/io/InputStream;"
        ));
        assert!(!class_reflection_shape("isArray", "()Ljava/lang/Class;"));
        assert!(!http_carrier_declaring_class(
            "org/example/MyHttpURLConnection"
        ));
        assert!(!http_carrier_declaring_class("java/net/URL"));
    }
}

/// G34-1 — the deliberate ABSENCES from [`force_native_over_real_jdk_bytecode`].
///
/// A test module for things that are not there needs a reason to exist, and
/// this is it: `G29-1` §6 recorded that its author could not tell whether the
/// newly registered `java/net/http/HttpHeaders` readers would ever run, because
/// the class is a real one with real bytecode and is not on the force list. The
/// answer (MEASURED, see the banner on that function) is that they run anyway —
/// the force list is not the gate. Someone who re-derives the question from
/// reading alone will reach for "add HttpHeaders to the list" as the fix, and
/// that would convert every `HttpHeaders` inline cache in the VM to
/// `VirtualNative` for real receivers too. These tests make the absence
/// deliberate rather than accidental.
#[cfg(test)]
mod force_list_deliberate_absences_tests {
    use super::force_native_over_real_jdk_bytecode as force;
    use super::real_protected_stub_class_common;

    /// MEASURED 2026-08-17 (`target-rel2`, `--jdk-only`, vs HotSpot
    /// 25.0.3+9-LTS): all five readers are registered `Bridge` by
    /// `net_phase_e.rs`, all five have real `Code`, none is here, and all five
    /// run — `invocations=300002` after a 300,000-iteration warm loop at one
    /// call site, every one tagged `bridge-ran-over-bytecode` by
    /// `--jdk-only-report`. `RJdkOptionalShape` is `checks=1418 PASS` on that
    /// binary, which is the same fact stated end to end.
    ///
    /// If this test ever fails, the entry that was added did NOT make a
    /// non-running native run; it changed which body real `HttpHeaders`
    /// receivers get on warm call sites. Read the banner before keeping it.
    #[test]
    fn http_headers_readers_are_deliberately_absent() {
        for (name, descriptor) in [
            ("map", "()Ljava/util/Map;"),
            ("firstValue", "(Ljava/lang/String;)Ljava/util/Optional;"),
            ("allValues", "(Ljava/lang/String;)Ljava/util/List;"),
            (
                "firstValueAsLong",
                "(Ljava/lang/String;)Ljava/util/OptionalLong;",
            ),
            ("toString", "()Ljava/lang/String;"),
        ] {
            assert!(
                !force("java/net/http/HttpHeaders", name, descriptor),
                "java/net/http/HttpHeaders.{name}{descriptor} is on the force \
                 list. It does not need to be: MEASURED, its registered Bridge \
                 already preempts the real JDK body at try_stackless_invoke \
                 step 1, cold and warm. See the banner on \
                 force_native_over_real_jdk_bytecode and G34-1."
            );
        }
    }

    /// `java/util/Optional` is the family `G29-1` reasoned FROM, and its
    /// reasoning was right for the wrong reason: it inferred from
    /// `invocations=244` that a native on a real-bytecode class wins. It does —
    /// but not because `Optional` is special, and not because anything about
    /// `Optional` is on this list. Twenty `Optional` triples are registered by
    /// `native-collections/src/lib.rs`, every one with
    /// `real_declaring_method.has_code = true`, and not one of them is here.
    #[test]
    fn optional_is_deliberately_absent_too() {
        for (name, descriptor) in [
            ("isPresent", "()Z"),
            ("get", "()Ljava/lang/Object;"),
            ("orElse", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ("toString", "()Ljava/lang/String;"),
            ("empty", "()Ljava/util/Optional;"),
        ] {
            assert!(
                !force("java/util/Optional", name, descriptor),
                "java/util/Optional.{name}{descriptor} was added to the force \
                 list. Its native already wins without it (MEASURED, G34-1); \
                 adding it only changes warm-call-site behaviour."
            );
        }
    }

    /// The negative control for the two tests above: this list is not empty and
    /// they are not passing because `force` answers `false` for everything.
    ///
    /// `java/util/ArrayList.size()I` is on it, and its comment says why — a
    /// `Map.values()` view is minted as an `ArrayList` that must re-sync
    /// against its source map on read, which the real `ArrayList` body cannot
    /// do. That is the shape of a justified entry: the native is not a
    /// duplicate of the JDK body, it services a receiver the JDK body cannot.
    #[test]
    fn the_list_is_not_vacuously_empty() {
        assert!(
            force("java/util/ArrayList", "size", "()I"),
            "the ArrayList map-view family must still be forced; without it the \
             two absence tests above prove nothing"
        );
        assert!(force("java/util/ArrayList", "get", "(I)Ljava/lang/Object;"));
        assert!(force(
            "java/util/HashMap$KeySet",
            "iterator",
            "()Ljava/util/Iterator;"
        ));
    }

    /// `java/util/ArrayList` is allow-listed by default and REVOKED by a
    /// fallback view mint, and the FORCE list is untouched in either state.
    ///
    /// Both directions are asserted from one test because the latch is a
    /// process-global: splitting them would make the pair order-dependent under
    /// the default parallel harness.
    #[test]
    fn arraylist_is_allow_listed_until_a_fallback_view_is_minted() {
        cratonvm_types::arraylist_view::reset_for_test();
        assert!(
            real_protected_stub_class_common("java/util/ArrayList"),
            "the default licenses the yield — see cratonvm_types::arraylist_view \
             for why the opposite default measured INERT"
        );

        cratonvm_types::arraylist_view::note_arraylist_classed_view_minted();
        assert!(
            !real_protected_stub_class_common("java/util/ArrayList"),
            "a view minted under java/util/ArrayList has to keep its native"
        );
        assert_eq!(
            cratonvm_types::arraylist_view::arraylist_view_fallback_count(),
            1
        );

        // The unconditional members are unaffected by the latch either way.
        assert!(real_protected_stub_class_common("java/util/Objects"));
        assert!(force("java/util/ArrayList", "size", "()I"));
        cratonvm_types::arraylist_view::reset_for_test();
    }
}
