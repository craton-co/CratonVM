//! STR719-3 (JIT round 19 proposal wave 8, lane str8): the TARGET names the
//! VM's by-name `instanceof` fallback can admit.
//!
//! `vm::runtime::interpreter::typecheck::synthetic_implements` answers
//! `checkcast` / `instanceof` / `aastore` / reflection (`Class.isInstance`,
//! `isAssignableFrom`, `cast`, through `NativeContext::synthetic_implements_declared`)
//! for the concrete stand-in classes the VM mints, after the class hierarchy
//! said no. Every one of its rules is keyed on an EXACT target name; none is a
//! prefix or substring rule on the target (those are all on the object's
//! name). So the set of targets it can ever answer `true` for is this finite
//! table, and a target outside it is answered `false` without the object's
//! name -- i.e. without the class-manager lock that lookup takes.
//!
//! The table is shared by the function (its opt-in early gate,
//! `CRATONVM_SYNTHETIC_IMPLEMENTS_TARGET_GATE`) and by the reflection fact
//! (`native-builtins` `class_facts_memo::NameFacts::is_synthetic_target`, the
//! `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_FALSE` rung), so the two cannot
//! drift. The vm test `the_synthetic_target_table_is_exactly_what_the_rules_admit`
//! scans the function's source for every string literal and proves both
//! directions over a corpus built from them: no rule admits a target outside
//! the table, and every table entry is admitted by some object.

/// Every target name `synthetic_implements` can answer `true` for.
pub const SYNTHETIC_IMPLEMENTS_TARGETS: &[&str] = &[
    "java/lang/invoke/MethodHandle",
    "java/lang/System$Logger",
    "java/lang/foreign/MemorySegment",
    "jdk/internal/foreign/AbstractMemorySegmentImpl",
    "java/lang/foreign/SegmentAllocator",
    "java/lang/management/BufferPoolMXBean",
    "java/lang/management/PlatformManagedObject",
    "jdk/internal/misc/VM$BufferPool",
    "java/util/Map$Entry",
    "java/util/Iterator",
    "java/io/Serializable",
    "java/lang/Cloneable",
    "java/lang/Iterable",
    "java/util/Collection",
    "java/util/List",
    "java/util/Set",
    "java/util/Queue",
    "java/util/Deque",
    "java/lang/Comparable",
    "java/lang/annotation/Annotation",
    "java/lang/Object",
];

/// May `synthetic_implements` answer `true` for some object and this target?
/// `false` is a proof that it answers `false` for every object.
pub fn synthetic_implements_may_admit(target_class_name: &str) -> bool {
    SYNTHETIC_IMPLEMENTS_TARGETS.contains(&target_class_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_no_duplicates_and_answers_its_own_names() {
        for (i, a) in SYNTHETIC_IMPLEMENTS_TARGETS.iter().enumerate() {
            assert!(synthetic_implements_may_admit(a), "{a}");
            for b in &SYNTHETIC_IMPLEMENTS_TARGETS[i + 1..] {
                assert_ne!(a, b);
            }
        }
        for other in [
            "",
            "java/util/HashMap",
            "java/lang/Runnable",
            "[I",
            "Foo$Bar",
        ] {
            assert!(!synthetic_implements_may_admit(other), "{other}");
        }
    }
}
